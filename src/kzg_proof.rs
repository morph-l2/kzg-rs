use core::num::NonZeroUsize;
use core::ops::Mul;

use crate::enums::KzgError;
use crate::trusted_setup::KzgSettings;
use crate::{
    dtypes::*, pairings_verify, BYTES_PER_BLOB, BYTES_PER_COMMITMENT, CHALLENGE_INPUT_SIZE,
    DOMAIN_STR_LENGTH, FIAT_SHAMIR_PROTOCOL_DOMAIN, MODULUS, NUM_FIELD_ELEMENTS_PER_BLOB,
};

use alloc::{string::ToString, vec::Vec};
use bls12_381::{pairing, G1Affine, G1Projective, G2Affine, G2Projective, Scalar};
use ff::derive::sbb;
use sha2::{Digest, Sha256};

fn safe_g1_affine_from_bytes(bytes: &Bytes48) -> Result<G1Affine, KzgError> {
    let g1 = G1Affine::from_compressed(&(bytes.clone().into()));
    if g1.is_none().into() {
        return Err(KzgError::BadArgs(
            "Failed to parse G1Affine from bytes".to_string(),
        ));
    }
    Ok(g1.unwrap())
}

pub(crate) fn safe_scalar_affine_from_bytes(bytes: &Bytes32) -> Result<Scalar, KzgError> {
    let lendian: [u8; 32] = Into::<[u8; 32]>::into(bytes.clone())
        .iter()
        .rev()
        .map(|&x| x)
        .collect::<Vec<u8>>()
        .try_into()
        .unwrap();
    let scalar = Scalar::from_bytes(&lendian);
    if scalar.is_none().into() {
        return Err(KzgError::BadArgs(
            "Failed to parse G1Affine from bytes".to_string(),
        ));
    }
    Ok(scalar.unwrap())
}

/// Return the Fiat-Shamir challenge required to verify `blob` and `commitment`.
fn compute_challenge(blob: &Blob, commitment: &G1Affine) -> Result<Scalar, KzgError> {
    let mut bytes = [0_u8; CHALLENGE_INPUT_SIZE];
    let mut offset = 0_usize;

    // Copy domain separator
    bytes[offset..DOMAIN_STR_LENGTH].copy_from_slice(FIAT_SHAMIR_PROTOCOL_DOMAIN.as_bytes());
    offset += DOMAIN_STR_LENGTH;

    // Copy polynomial degree (16-bytes, big-endian)
    bytes[offset..offset + 8].copy_from_slice(&0_u64.to_be_bytes());
    offset += 8;
    bytes[offset..offset + 8].copy_from_slice(&(NUM_FIELD_ELEMENTS_PER_BLOB as u64).to_be_bytes());
    offset += 8;

    // Copy blob
    bytes[offset..offset + BYTES_PER_BLOB].copy_from_slice(blob.as_slice());
    offset += BYTES_PER_BLOB;

    // Copy commitment
    bytes[offset..offset + BYTES_PER_COMMITMENT].copy_from_slice(&commitment.to_compressed());
    offset += BYTES_PER_COMMITMENT;

    /* Make sure we wrote the entire buffer */

    if offset != CHALLENGE_INPUT_SIZE {
        return Err(KzgError::InvalidBytesLength(format!(
            "The challenge should be {} length, but was {}",
            CHALLENGE_INPUT_SIZE, offset,
        )));
    }

    let evaluation: [u8; 32] = Sha256::digest(bytes).into();

    Ok(scalar_from_bytes_unchecked(evaluation))
}

fn scalar_from_bytes_unchecked(bytes: [u8; 32]) -> Scalar {
    scalar_from_u64_array_unchecked([
        u64::from_be_bytes(<[u8; 8]>::try_from(&bytes[0..8]).unwrap()),
        u64::from_be_bytes(<[u8; 8]>::try_from(&bytes[8..16]).unwrap()),
        u64::from_be_bytes(<[u8; 8]>::try_from(&bytes[16..24]).unwrap()),
        u64::from_be_bytes(<[u8; 8]>::try_from(&bytes[24..32]).unwrap()),
    ])
}

fn scalar_from_u64_array_unchecked(array: [u64; 4]) -> Scalar {
    // Try to subtract the modulus
    let (_, borrow) = sbb(array[0], MODULUS[0], 0);
    let (_, borrow) = sbb(array[1], MODULUS[1], borrow);
    let (_, borrow) = sbb(array[2], MODULUS[2], borrow);
    let (_, _borrow) = sbb(array[3], MODULUS[3], borrow);

    Scalar::from_raw([array[3], array[2], array[1], array[0]])
}

/// Evaluates a polynomial in evaluation form at a given point
fn evaluate_polynomial_in_evaluation_form(
    polynomial: Vec<Scalar>,
    x: Scalar,
    kzg_settings: &KzgSettings,
) -> Result<Scalar, KzgError> {
    if polynomial.len() != NUM_FIELD_ELEMENTS_PER_BLOB {
        return Err(KzgError::InvalidBytesLength(
            "The polynomial length is incorrect".to_string(),
        ));
    }

    let mut inverses_in = vec![Scalar::default(); NUM_FIELD_ELEMENTS_PER_BLOB];
    let mut inverses = vec![Scalar::default(); NUM_FIELD_ELEMENTS_PER_BLOB];
    let roots_of_unity = kzg_settings.roots_of_unity;

    for i in 0..NUM_FIELD_ELEMENTS_PER_BLOB {
        // If the point to evaluate at is one of the evaluation points by which
        // the polynomial is given, we can just return the result directly.
        // Note that special-casing this is necessary, as the formula below
        // would divide by zero otherwise.
        if x == roots_of_unity[i] {
            return Ok(polynomial[i]);
        }
        inverses_in[i] = x - roots_of_unity[i];
    }

    batch_inversion(
        &mut inverses,
        &inverses_in,
        NonZeroUsize::new(NUM_FIELD_ELEMENTS_PER_BLOB).unwrap(),
    )?;

    let mut out = Scalar::zero();

    for i in 0..NUM_FIELD_ELEMENTS_PER_BLOB {
        out += (inverses[i] * roots_of_unity[i]) * polynomial[i];
    }

    out *= Scalar::from(NUM_FIELD_ELEMENTS_PER_BLOB as u64)
        .invert()
        .unwrap();
    out *= x.pow(&[NUM_FIELD_ELEMENTS_PER_BLOB as u64, 0, 0, 0]) - Scalar::one();

    Ok(out)
}

fn batch_inversion(out: &mut [Scalar], a: &[Scalar], len: NonZeroUsize) -> Result<(), KzgError> {
    if a == out {
        return Err(KzgError::BadArgs(
            "Destination is the same as source".to_string(),
        ));
    }

    let mut accumulator = Scalar::one();

    for i in 0..len.into() {
        out[i] = accumulator;
        accumulator = accumulator.mul(&a[i]);
    }

    if accumulator == Scalar::zero() {
        return Err(KzgError::BadArgs("Zero input".to_string()));
    }

    accumulator = accumulator.invert().unwrap();

    for i in (0..len.into()).rev() {
        out[i] *= accumulator;
        accumulator *= a[i];
    }

    Ok(())
}

fn verify_kzg_proof_impl(
    commitment: G1Affine,
    z: Scalar,
    y: Scalar,
    proof: G1Affine,
    kzg_settings: &KzgSettings,
) -> Result<bool, KzgError> {
    let x = G2Projective::generator() * z;
    let x_minus_z = kzg_settings.g2_points[1] - x;

    let y = G1Projective::generator() * y;
    let p_minus_y = commitment - y;

    // Verify: P - y = Q * (X - z)
    Ok(pairings_verify(
        p_minus_y.into(),
        G2Projective::generator().into(),
        proof,
        x_minus_z.into(),
    ))
}

fn validate_batched_input(commitments: &[G1Affine], proofs: &[G1Affine]) -> Result<(), KzgError> {
    let invalid_commitment = commitments.iter().any(|commitment| {
        !bool::from(commitment.is_identity()) && !bool::from(commitment.is_on_curve())
    });

    let invalid_proof = proofs
        .iter()
        .any(|proof| !bool::from(proof.is_identity()) && !bool::from(proof.is_on_curve()));

    if invalid_commitment {
        return Err(KzgError::BadArgs("Invalid commitment".to_string()));
    }
    if invalid_proof {
        return Err(KzgError::BadArgs("Invalid proof".to_string()));
    }

    Ok(())
}

fn compute_challenges_and_evaluate_polynomial(
    blobs: Vec<Blob>,
    commitments: &[G1Affine],
    kzg_settings: &KzgSettings,
) -> Result<(Vec<Scalar>, Vec<Scalar>), KzgError> {
    let mut evaluation_challenges = Vec::with_capacity(blobs.len());
    let mut ys = Vec::with_capacity(blobs.len());

    for i in 0..blobs.len() {
        let polynomial = blobs[i].as_polynomial()?;
        let evaluation_challenge = compute_challenge(&blobs[i], &commitments[i])?;
        let y =
            evaluate_polynomial_in_evaluation_form(polynomial, evaluation_challenge, kzg_settings)?;

        evaluation_challenges.push(evaluation_challenge);
        ys.push(y);
    }

    Ok((evaluation_challenges, ys))
}

pub struct KzgProof {}

impl KzgProof {
    pub fn verify_kzg_proof(
        commitment_bytes: &Bytes48,
        z_bytes: &Bytes32,
        y_bytes: &Bytes32,
        proof_bytes: &Bytes48,
        kzg_settings: &KzgSettings,
    ) -> Result<bool, KzgError> {
        let z = match safe_scalar_affine_from_bytes(z_bytes) {
            Ok(z) => z,
            Err(e) => {
                return Err(e);
            }
        };
        let y = match safe_scalar_affine_from_bytes(y_bytes) {
            Ok(y) => y,
            Err(e) => {
                return Err(e);
            }
        };
        let commitment = match safe_g1_affine_from_bytes(commitment_bytes) {
            Ok(g1) => g1,
            Err(e) => {
                return Err(e);
            }
        };
        let proof = match safe_g1_affine_from_bytes(proof_bytes) {
            Ok(g1) => g1,
            Err(e) => {
                return Err(e);
            }
        };

        let g2_x = G2Affine::generator() * z;
        let x_minus_z = kzg_settings.g2_points[1] - g2_x;

        let g1_y = G1Affine::generator() * y;
        let p_minus_y = commitment - g1_y;

        Ok(
            pairing(&p_minus_y.into(), &G2Affine::generator())
                == pairing(&proof, &x_minus_z.into()),
        )
    }

    pub fn verify_kzg_proof_batch(
        commitments: &[G1Affine],
        zs: &[Scalar],
        ys: &[Scalar],
        proofs: &[G1Affine],
        kzg_settings: &KzgSettings,
    ) -> Result<bool, KzgError> {
        todo!()
    }

    pub fn verify_blob_kzg_proof(
        blob: Blob,
        commitment_bytes: &Bytes48,
        proof_bytes: &Bytes48,
        kzg_settings: &KzgSettings,
    ) -> Result<bool, KzgError> {
        let commitment = safe_g1_affine_from_bytes(commitment_bytes)?;
        let polynomial = blob.as_polynomial()?;
        let proof = safe_g1_affine_from_bytes(proof_bytes)?;

        // Compute challenge for the blob/commitment
        let evaluation_challenge = compute_challenge(&blob, &commitment)?;

        let y =
            evaluate_polynomial_in_evaluation_form(polynomial, evaluation_challenge, kzg_settings)?;

        verify_kzg_proof_impl(commitment, evaluation_challenge, y, proof, kzg_settings)
    }

    pub fn verify_blob_kzg_proof_batch(
        blobs: Vec<Blob>,
        commitments_bytes: Vec<Bytes48>,
        proofs_bytes: Vec<Bytes48>,
        kzg_settings: &KzgSettings,
    ) -> Result<bool, KzgError> {
        // Exit early if we are given zero blobs
        if blobs.is_empty() {
            return Ok(true);
        }

        // For a single blob, just do a regular single verification
        if blobs.len() == 1 {
            return Self::verify_blob_kzg_proof(
                blobs[0].clone(),
                &commitments_bytes[0],
                &proofs_bytes[0],
                kzg_settings,
            );
        }

        if blobs.len() != commitments_bytes.len() {
            return Err(KzgError::InvalidBytesLength(
                "Invalid commitments length".to_string(),
            ));
        }

        if blobs.len() != proofs_bytes.len() {
            return Err(KzgError::InvalidBytesLength(
                "Invalid proofs length".to_string(),
            ));
        }

        let commitments = commitments_bytes
            .iter()
            .map(safe_g1_affine_from_bytes)
            .collect::<Result<Vec<_>, _>>()?;

        let proofs = proofs_bytes
            .iter()
            .map(safe_g1_affine_from_bytes)
            .collect::<Result<Vec<_>, _>>()?;

        validate_batched_input(&commitments, &proofs)?;

        let (evaluation_challenges, ys) =
            compute_challenges_and_evaluate_polynomial(blobs, &commitments, kzg_settings)?;

        Self::verify_kzg_proof_batch(
            &commitments,
            &evaluation_challenges,
            &ys,
            &proofs,
            kzg_settings,
        )
    }
}

#[cfg(feature = "std")]
#[cfg(test)]
mod tests {
    use super::*;
    use serde_derive::Deserialize;
    use std::{fs, path::PathBuf};

    const VERIFY_KZG_PROOF_TESTS: &str = "tests/verify_kzg_proof/*/*";
    const VERIFY_BLOB_KZG_PROOF_TESTS: &str = "tests/verify_blob_kzg_proof/*/*";

    #[derive(Debug, Deserialize)]
    pub struct Input<'a> {
        commitment: &'a str,
        z: &'a str,
        y: &'a str,
        proof: &'a str,
    }

    impl Input<'_> {
        pub fn get_commitment(&self) -> Result<Bytes48, KzgError> {
            Bytes48::from_hex(self.commitment)
        }

        pub fn get_z(&self) -> Result<Bytes32, KzgError> {
            Bytes32::from_hex(self.z)
        }

        pub fn get_y(&self) -> Result<Bytes32, KzgError> {
            Bytes32::from_hex(self.y)
        }

        pub fn get_proof(&self) -> Result<Bytes48, KzgError> {
            Bytes48::from_hex(self.proof)
        }
    }

    #[derive(Debug, Deserialize)]
    pub struct Test<I> {
        pub input: I,
        output: Option<bool>,
    }

    impl<I> Test<I> {
        pub fn get_output(&self) -> Option<bool> {
            self.output
        }
    }

    #[test]
    #[cfg(feature = "cache")]
    fn test_verify_kzg_proof() {
        let kzg_settings = KzgSettings::load_trusted_setup_file().unwrap();
        let test_files: Vec<PathBuf> = glob::glob(VERIFY_KZG_PROOF_TESTS)
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        for test_file in test_files {
            let yaml_data = fs::read_to_string(test_file.clone()).unwrap();
            let test: Test<Input> = serde_yaml::from_str(&yaml_data).unwrap();
            let (Ok(commitment), Ok(z), Ok(y), Ok(proof)) = (
                test.input.get_commitment(),
                test.input.get_z(),
                test.input.get_y(),
                test.input.get_proof(),
            ) else {
                assert!(test.get_output().is_none());
                continue;
            };

            let result = KzgProof::verify_kzg_proof(&commitment, &z, &y, &proof, &kzg_settings);
            match result {
                Ok(result) => {
                    assert_eq!(result, test.get_output().unwrap_or(false));
                }
                Err(e) => {
                    assert!(test.get_output().is_none());
                    eprintln!("Error: {:?}", e);
                }
            }
        }
    }

    #[derive(Debug, Deserialize)]
    pub struct BlobInput<'a> {
        blob: &'a str,
        commitment: &'a str,
        proof: &'a str,
    }

    impl BlobInput<'_> {
        pub fn get_blob(&self) -> Result<Blob, KzgError> {
            Blob::from_hex(self.blob)
        }

        pub fn get_commitment(&self) -> Result<Bytes48, KzgError> {
            Bytes48::from_hex(self.commitment)
        }

        pub fn get_proof(&self) -> Result<Bytes48, KzgError> {
            Bytes48::from_hex(self.proof)
        }
    }

    #[test]
    #[cfg(feature = "cache")]
    fn test_verify_blob_kzg_proof() {
        let kzg_settings = KzgSettings::load_trusted_setup_file().unwrap();
        let test_files: Vec<PathBuf> = glob::glob(VERIFY_BLOB_KZG_PROOF_TESTS)
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        for test_file in test_files {
            let yaml_data = fs::read_to_string(test_file.clone()).unwrap();
            let test: Test<BlobInput> = serde_yaml::from_str(&yaml_data).unwrap();
            let (Ok(blob), Ok(commitment), Ok(proof)) = (
                test.input.get_blob(),
                test.input.get_commitment(),
                test.input.get_proof(),
            ) else {
                assert!(test.get_output().is_none());
                continue;
            };

            let result = KzgProof::verify_blob_kzg_proof(blob, &commitment, &proof, &kzg_settings);
            match result {
                Ok(result) => {
                    assert_eq!(result, test.get_output().unwrap_or(false));
                }
                Err(e) => {
                    assert!(test.get_output().is_none());
                    eprintln!("Error: {:?}", e);
                }
            }
        }
    }

    #[test]
    fn test_compute_challenge() {
        let test_file = "tests/verify_blob_kzg_proof/verify_blob_kzg_proof_case_correct_proof_fb324bc819407148/data.yaml";

        let yaml_data = fs::read_to_string(test_file).unwrap();
        let test: Test<BlobInput> = serde_yaml::from_str(&yaml_data).unwrap();
        let blob = test.input.get_blob().unwrap();
        let commitment = safe_g1_affine_from_bytes(&test.input.get_commitment().unwrap()).unwrap();

        let evaluation_challenge = compute_challenge(&blob, &commitment).unwrap();

        assert_eq!(
            format!("{evaluation_challenge}"),
            "0x4f00eef944a21cb9f3ac3390702621e4bbf1198767c43c0fb9c8e9923bfbb31a"
        )
    }

    #[test]
    #[cfg(feature = "cache")]
    fn test_evaluate_polynomial_in_evaluation_form() {
        let test_file = "tests/verify_blob_kzg_proof/verify_blob_kzg_proof_case_correct_proof_19b3f3f8c98ea31e/data.yaml";

        let yaml_data = fs::read_to_string(test_file).unwrap();
        let test: Test<BlobInput> = serde_yaml::from_str(&yaml_data).unwrap();
        let kzg_settings = KzgSettings::load_trusted_setup_file().unwrap();
        let blob = test.input.get_blob().unwrap();
        let polynomial = blob.as_polynomial().unwrap();

        let evaluation_challenge = scalar_from_bytes_unchecked(
            Bytes32::from_hex("0x637c904d316955b7282f980433d5cd9f40d0533c45d0a233c009bc7fe28b92e3")
                .unwrap()
                .into(),
        );

        let y =
            evaluate_polynomial_in_evaluation_form(polynomial, evaluation_challenge, &kzg_settings)
                .unwrap();

        assert_eq!(
            format!("{y}"),
            "0x1bdfc5da40334b9c51220e8cbea1679c20a7f32dd3d7f3c463149bb4b41a7d18"
        );
    }
}
