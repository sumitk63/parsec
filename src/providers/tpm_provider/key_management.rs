// Copyright 2020 Contributors to the Parsec project.
// SPDX-License-Identifier: Apache-2.0
use super::utils;
use super::utils::{PasswordContext, RsaPublicKey};
use super::TpmProvider;
use crate::authenticators::ApplicationName;
use crate::key_info_managers;
use crate::key_info_managers::KeyTriple;
use crate::key_info_managers::{KeyInfo, ManageKeyInfo};
use log::error;
use parsec_interface::operations::psa_key_attributes::*;
use parsec_interface::operations::{
    psa_destroy_key, psa_export_public_key, psa_generate_key, psa_import_key,
};
use parsec_interface::requests::{ProviderID, ResponseStatus, Result};
use picky_asn1::wrapper::IntegerAsn1;

// Public exponent value for all RSA keys.
const PUBLIC_EXPONENT: [u8; 3] = [0x01, 0x00, 0x01];
const AUTH_VAL_LEN: usize = 32;

// Inserts a new mapping in the Key Info manager that stores the PasswordContext.
fn insert_password_context(
    store_handle: &mut dyn ManageKeyInfo,
    key_triple: KeyTriple,
    password_context: PasswordContext,
    key_attributes: KeyAttributes,
) -> Result<()> {
    let error_storing = |e| Err(key_info_managers::to_response_status(e));

    let key_info = KeyInfo {
        id: bincode::serialize(&password_context)?,
        attributes: key_attributes,
    };

    if store_handle
        .insert(key_triple, key_info)
        .or_else(error_storing)?
        .is_some()
    {
        error!("Inserting a mapping in the Key Info Manager that would overwrite an existing one.");
        Err(ResponseStatus::PsaErrorAlreadyExists)
    } else {
        Ok(())
    }
}

// Gets a PasswordContext mapping to the KeyTriple given.
pub fn get_password_context(
    store_handle: &dyn ManageKeyInfo,
    key_triple: KeyTriple,
) -> Result<(PasswordContext, KeyAttributes)> {
    let key_info = store_handle
        .get(&key_triple)
        .or_else(|e| Err(key_info_managers::to_response_status(e)))?
        .ok_or_else(|| {
            error!(
                "Key triple \"{}\" does not exist in the Key Info Manager.",
                key_triple
            );
            ResponseStatus::PsaErrorDoesNotExist
        })?;
    Ok((bincode::deserialize(&key_info.id)?, key_info.attributes))
}

impl TpmProvider {
    pub(super) fn psa_generate_key_internal(
        &self,
        app_name: ApplicationName,
        op: psa_generate_key::Operation,
    ) -> Result<psa_generate_key::Result> {
        if op.attributes.key_type != KeyType::RsaKeyPair {
            error!("The TPM provider currently only supports creating RSA key pairs.");
            return Err(ResponseStatus::PsaErrorNotSupported);
        }

        let key_name = op.key_name;
        let attributes = op.attributes;
        let key_triple = KeyTriple::new(app_name, ProviderID::Tpm, key_name);
        // This should never panic on 32 bits or more machines.
        let key_size = std::convert::TryFrom::try_from(op.attributes.key_bits)
            .expect("Conversion to usize failed.");

        let mut store_handle = self
            .key_info_store
            .write()
            .expect("Key store lock poisoned");
        let mut esapi_context = self
            .esapi_context
            .lock()
            .expect("ESAPI Context lock poisoned");

        let (key_context, auth_value) = esapi_context
            .create_rsa_signing_key(key_size, AUTH_VAL_LEN)
            .or_else(|e| {
                error!("Error creating a RSA signing key: {}.", e);
                Err(utils::to_response_status(e))
            })?;

        insert_password_context(
            &mut *store_handle,
            key_triple,
            PasswordContext {
                context: key_context,
                auth_value,
            },
            attributes,
        )?;

        Ok(psa_generate_key::Result {})
    }

    pub(super) fn psa_import_key_internal(
        &self,
        app_name: ApplicationName,
        op: psa_import_key::Operation,
    ) -> Result<psa_import_key::Result> {
        if op.attributes.key_type != KeyType::RsaPublicKey {
            error!("The TPM provider currently only supports importing RSA public key.");
            return Err(ResponseStatus::PsaErrorNotSupported);
        }

        let key_name = op.key_name;
        let attributes = op.attributes;
        let key_triple = KeyTriple::new(app_name, ProviderID::Tpm, key_name);
        let key_data = op.data;

        let mut store_handle = self
            .key_info_store
            .write()
            .expect("Key store lock poisoned");
        let mut esapi_context = self
            .esapi_context
            .lock()
            .expect("ESAPI Context lock poisoned");

        let public_key: RsaPublicKey = picky_asn1_der::from_bytes(&key_data).or_else(|err| {
            error!("Could not deserialise key elements: {}.", err);
            Err(ResponseStatus::PsaErrorInvalidArgument)
        })?;

        if public_key.modulus.is_negative() || public_key.public_exponent.is_negative() {
            error!("Only positive modulus and public exponent are supported.");
            return Err(ResponseStatus::PsaErrorInvalidArgument);
        }

        if public_key.public_exponent.as_unsigned_bytes_be() != PUBLIC_EXPONENT {
            error!("The TPM Provider only supports 0x101 as public exponent for RSA public keys, {:?} given.", public_key.public_exponent.as_unsigned_bytes_be());
            return Err(ResponseStatus::PsaErrorNotSupported);
        }
        let key_data = public_key.modulus.as_unsigned_bytes_be();
        let len = key_data.len();

        let key_bits = attributes.key_bits;
        if key_bits != 0 && len * 8 != key_bits as usize {
            error!("If the key_bits field is non-zero (value is {}) it must be equal to the size of the key in data.", attributes.key_bits);
            return Err(ResponseStatus::PsaErrorInvalidArgument);
        }

        if len != 128 && len != 256 {
            error!(
                "The TPM provider only supports 1024 and 2048 bits RSA public keys ({} bits given).",
                len * 8
            );
            return Err(ResponseStatus::PsaErrorNotSupported);
        }

        let pub_key_context = esapi_context
            .load_external_rsa_public_key(&key_data)
            .or_else(|e| {
                error!("Error creating a RSA signing key: {}.", e);
                Err(utils::to_response_status(e))
            })?;

        insert_password_context(
            &mut *store_handle,
            key_triple,
            PasswordContext {
                context: pub_key_context,
                auth_value: Vec::new(),
            },
            attributes,
        )?;

        Ok(psa_import_key::Result {})
    }

    pub(super) fn psa_export_public_key_internal(
        &self,
        app_name: ApplicationName,
        op: psa_export_public_key::Operation,
    ) -> Result<psa_export_public_key::Result> {
        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::Tpm, key_name);

        let store_handle = self.key_info_store.read().expect("Key store lock poisoned");
        let mut esapi_context = self
            .esapi_context
            .lock()
            .expect("ESAPI Context lock poisoned");

        let (password_context, _key_attributes) = get_password_context(&*store_handle, key_triple)?;

        let pub_key_data = esapi_context
            .read_public_key(password_context.context)
            .or_else(|e| {
                error!("Error reading a public key: {}.", e);
                Err(utils::to_response_status(e))
            })?;

        let key = RsaPublicKey {
            // To produce a valid ASN.1 RSAPublicKey structure, 0x00 is put in front of the positive
            // modulus if highest significant bit is one, to differentiate it from a negative number.
            modulus: IntegerAsn1::from_unsigned_bytes_be(pub_key_data),
            public_exponent: IntegerAsn1::from_signed_bytes_be(PUBLIC_EXPONENT.to_vec()),
        };
        let key_data = picky_asn1_der::to_vec(&key).or_else(|err| {
            error!("Could not serialise key elements: {}.", err);
            Err(ResponseStatus::PsaErrorCommunicationFailure)
        })?;

        Ok(psa_export_public_key::Result { data: key_data })
    }

    pub(super) fn psa_destroy_key_internal(
        &self,
        app_name: ApplicationName,
        op: psa_destroy_key::Operation,
    ) -> Result<psa_destroy_key::Result> {
        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::Tpm, key_name);
        let mut store_handle = self
            .key_info_store
            .write()
            .expect("Key store lock poisoned");

        let error_closure = |e| Err(key_info_managers::to_response_status(e));
        if store_handle
            .remove(&key_triple)
            .or_else(error_closure)?
            .is_none()
        {
            error!(
                "Key triple \"{}\" does not exist in the Key Info Manager.",
                key_triple
            );
            Err(ResponseStatus::PsaErrorDoesNotExist)
        } else {
            Ok(psa_destroy_key::Result {})
        }
    }
}
