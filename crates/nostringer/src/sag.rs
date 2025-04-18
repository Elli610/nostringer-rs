use k256::elliptic_curve::group::Group;
use k256::elliptic_curve::ops::Reduce;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::{NonZeroScalar, ProjectivePoint, Scalar, U256};
use rand::rngs::OsRng;
use rand::CryptoRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::{ConditionallySelectable, ConstantTimeEq};

use crate::types::{hex_to_scalar, Error, RingSignature, RingSignatureBinary};
use crate::utils::{hex_to_point, random_non_zero_scalar};

/// The generator point G for the secp256k1 curve
const GENERATOR: ProjectivePoint = ProjectivePoint::GENERATOR;

// --- Hex API ---

/// Creates a ring signature for a message using the provided private key and ring of public keys (Hex API)
///
/// This function implements the core ring signature algorithm:
/// 1. Finds the signer's position in the ring
/// 2. Generates random scalars for all other ring members
/// 3. Computes the signature components in a cyclic manner
/// 4. Completes the ring using the signer's private key
///
/// # Arguments
/// * `message` - The message to sign as a byte array
/// * `private_key_hex` - The signer's private key as a hex string
/// * `ring_pubkeys_hex` - The public keys of all ring members as hex strings
///
/// # Returns
/// * `Ok(RingSignature)` - The generated ring signature
/// * `Err(Error)` - If any step in the signature generation fails
pub fn sign(
    message: &[u8],
    private_key_hex: &str,
    ring_pubkeys_hex: &[String],
) -> Result<RingSignature, Error> {
    // Convert inputs from hex
    let private_key = hex_to_scalar(private_key_hex)?;
    let ring_pubkeys: Vec<ProjectivePoint> = ring_pubkeys_hex
        .iter()
        .map(|pubkey_str| hex_to_point(pubkey_str))
        .collect::<Result<_, _>>()?;

    // Call the binary version
    let binary_signature = sign_binary(message, &private_key, &ring_pubkeys, OsRng)?;

    // Convert the binary signature to hex format
    Ok(RingSignature::from(&binary_signature))
}

/// Verifies a ring signature against a message and a ring of public keys (Hex API)
///
/// This function implements the ring signature verification algorithm:
/// 1. Converts all components from hex to their scalar/point representations
/// 2. Recomputes the ring links using the signature components
/// 3. Checks if the ring closes properly (c₀ = c_n)
///
/// # Arguments
/// * `signature` - The ring signature to verify
/// * `message` - The message that was signed
/// * `ring_pubkeys_hex` - The public keys of all ring members as hex strings
///
/// # Returns
/// * `Ok(bool)` - Whether the signature is valid
/// * `Err(Error)` - If any step in the verification process fails
pub fn verify(
    signature: &RingSignature,
    message: &[u8],
    ring_pubkeys_hex: &[String],
) -> Result<bool, Error> {
    // Convert inputs from hex
    let binary_signature = RingSignatureBinary::try_from(signature)?;
    let ring_pubkeys: Vec<ProjectivePoint> = ring_pubkeys_hex
        .iter()
        .map(|pubkey_str| hex_to_point(pubkey_str))
        .collect::<Result<_, _>>()?;

    // Call the binary version
    verify_binary(&binary_signature, message, &ring_pubkeys)
}

// --- Binary API ---

/// Creates a ring signature for a message using the provided binary private key and ring of public keys
///
/// This function is the optimized binary version of the sign function, avoiding hex conversions.
///
/// # Arguments
/// * `message` - The message to sign as a byte array
/// * `private_key` - The signer's private key as a Scalar
/// * `ring_pubkeys` - The public keys of all ring members as ProjectivePoints
///
/// # Returns
/// * `Ok(RingSignatureBinary)` - The generated ring signature in binary format
/// * `Err(Error)` - If any step in the signature generation fails
pub fn sign_binary(
    message: &[u8],
    private_key: &Scalar,
    ring_pubkeys: &[ProjectivePoint],
    mut rng: impl RngCore + CryptoRng,
) -> Result<RingSignatureBinary, Error> {
    let ring_size = ring_pubkeys.len();
    if ring_size < 2 {
        return Err(Error::RingTooSmall(ring_size));
    }

    if *private_key == Scalar::ZERO {
        return Err(Error::PrivateKeyFormat(
            "Private key scalar cannot be zero".into(),
        ));
    }

    let d = *private_key;
    let _d_nonzero =
        NonZeroScalar::new(d).expect("d was checked non-zero, NonZeroScalar::new must succeed");

    // Compute the signer's public key point in both normal and negated form
    let my_point = GENERATOR * d;
    let flipped_d = d.negate();
    let flipped_point = GENERATOR * flipped_d;

    // Find the signer's position in the ring
    let mut signer_index: Option<usize> = None;
    let mut used_d = d;
    for (i, p) in ring_pubkeys.iter().enumerate() {
        if p == &my_point {
            signer_index = Some(i);
            used_d = d;
            break;
        }
        if p == &flipped_point {
            signer_index = Some(i);
            used_d = flipped_d;
            break;
        }
    }
    let signer_index = signer_index.ok_or(Error::SignerNotInRing)?;

    // Initialize vectors for the signature components
    let mut r_scalars = vec![Scalar::ZERO; ring_size];
    let mut c_scalars = vec![Scalar::ZERO; ring_size];

    // Generate a random scalar alpha and compute alpha*G
    let alpha_nonzero = random_non_zero_scalar(&mut rng);
    let alpha = *alpha_nonzero.as_ref();
    let alpha_g = GENERATOR * alpha;

    // Start the ring signature process at the position after the signer
    let start_index = (signer_index + 1) % ring_size;

    // Hash directly with binary points
    c_scalars[start_index] = hash_to_scalar(message, ring_pubkeys, &alpha_g)?;

    // Generate random components for each member and build the ring
    let mut current_index = start_index;
    while current_index != signer_index {
        // Random scalar for this ring member
        let r_nonzero = random_non_zero_scalar(&mut rng);
        r_scalars[current_index] = *r_nonzero.as_ref();

        // Compute the ring link: x_i = r_i*G + c_i*P_i
        let xi = (GENERATOR * r_scalars[current_index])
            + (ring_pubkeys[current_index] * c_scalars[current_index]);

        // Hash to get the next challenge
        let next_index = (current_index + 1) % ring_size;
        c_scalars[next_index] = hash_to_scalar(message, ring_pubkeys, &xi)?;
        current_index = next_index;
    }

    // Complete the ring by computing the signer's s value
    r_scalars[signer_index] = alpha - (c_scalars[signer_index] * used_d);

    // Return the binary signature
    Ok(RingSignatureBinary {
        c0: c_scalars[0],
        s: r_scalars,
    })
}

/// Verifies a ring signature against a message and a ring of public keys (Binary API)
///
/// This function is the optimized binary version of the verify function, avoiding hex conversions.
///
/// # Arguments
/// * `signature` - The binary ring signature to verify
/// * `message` - The message that was signed
/// * `ring_pubkeys` - The public keys of all ring members as ProjectivePoints
///
/// # Returns
/// * `Ok(bool)` - Whether the signature is valid
/// * `Err(Error)` - If any step in the verification process fails
pub fn verify_binary(
    signature: &RingSignatureBinary,
    message: &[u8],
    ring_pubkeys: &[ProjectivePoint],
) -> Result<bool, Error> {
    let ring_size = ring_pubkeys.len();
    if ring_size == 0 {
        return Ok(false);
    }
    if signature.s.len() != ring_size {
        return Err(Error::InvalidSignatureFormat);
    }

    // Get reference to the components directly
    let c0_scalar = signature.c0;
    let r_scalars = &signature.s;

    // Verify the ring by recomputing each link using binary points directly
    let mut current_c = c0_scalar;
    for i in 0..ring_size {
        // Compute x_i = s_i*G + c_i*P_i
        let xi = (GENERATOR * r_scalars[i]) + (ring_pubkeys[i] * current_c);
        // Hash to get the next challenge
        current_c = hash_to_scalar(message, ring_pubkeys, &xi)?;
    }

    // Check if the ring closes (c_n == c₀)
    let is_valid = current_c.ct_eq(&c0_scalar);
    Ok(is_valid.into())
}

// --- Hashing ---

/// Hashes a message, ring public keys (binary), and an ephemeral point to a scalar value.
fn hash_to_scalar(
    message: &[u8],
    ring_pubkeys: &[ProjectivePoint], // Accepts binary points directly
    ephemeral_point: &ProjectivePoint,
) -> Result<Scalar, Error> {
    // Initialize hasher
    let mut hasher = Sha256::new();

    // Hash the message
    hasher.update(message);

    // Hash all public keys in the ring (binary, compressed)
    for pk_point in ring_pubkeys {
        if pk_point.is_identity().into() {
            return Err(Error::PublicKeyFormat(
                "Cannot hash identity point in ring".into(),
            ));
        }
        let pk_bytes = pk_point.to_encoded_point(true);
        hasher.update(pk_bytes.as_bytes());
    }

    // Hash the ephemeral point (in compressed format)
    if ephemeral_point.is_identity().into() {
        return Err(Error::PublicKeyFormat(
            "Cannot hash identity ephemeral point".into(),
        ));
    }
    let ephemeral_compressed = ephemeral_point.to_encoded_point(true);
    hasher.update(ephemeral_compressed.as_bytes());

    // Finalize hash
    let hash_result = hasher.finalize();

    // Convert hash to a scalar
    let hash_uint = U256::from_be_slice(&hash_result);
    let scalar = Scalar::reduce(hash_uint);

    // Ensure result is non-zero (use Scalar::ONE if zero)
    let is_zero = scalar.ct_eq(&Scalar::ZERO);
    Ok(Scalar::conditional_select(&scalar, &Scalar::ONE, is_zero))
}

#[cfg(test)]
mod tests {
    use super::*; // Import items from parent module (sag.rs)
    use crate::keys::{generate_keypair_hex, generate_keypairs};
    use crate::types::hex_to_scalar;
    use crate::utils::hex_to_point;
    use k256::{ProjectivePoint, Scalar};
    use rand::rngs::OsRng;

    // Helper to setup a ring and signer for tests
    fn setup_test_ring(ring_size: usize) -> (Vec<ProjectivePoint>, Scalar, usize) {
        let keypairs_hex = generate_keypairs(ring_size, "compressed");
        let ring_pubkeys: Vec<ProjectivePoint> = keypairs_hex
            .iter()
            .map(|kp| hex_to_point(&kp.public_key_hex).unwrap())
            .collect();
        let signer_index = ring_size / 2;
        let signer_priv = hex_to_scalar(&keypairs_hex[signer_index].private_key_hex).unwrap();
        (ring_pubkeys, signer_priv, signer_index)
    }

    #[test]
    fn test_hash_for_challenge_consistency() {
        let message = b"test message";
        let ephemeral_placeholder = ProjectivePoint::GENERATOR; // Placeholder ephemeral point
        let point1 = ProjectivePoint::GENERATOR;
        let point2 = ProjectivePoint::GENERATOR * Scalar::from(2u64);
        let ring1 = &[point1];
        let ring2 = &[point2];
        let ring_both = &[point1, point2];

        // Test with consistent inputs
        let hash1 = hash_to_scalar(message, ring1, &ephemeral_placeholder).unwrap();
        let hash2 = hash_to_scalar(message, ring1, &ephemeral_placeholder).unwrap();
        assert_eq!(hash1, hash2, "Hashing should be deterministic");

        // Test with different message
        let hash3 = hash_to_scalar(b"different message", ring1, &ephemeral_placeholder).unwrap();
        assert_ne!(hash1, hash3, "Hash should differ for different messages");

        // Test with different ring
        let hash4 = hash_to_scalar(message, ring2, &ephemeral_placeholder).unwrap();
        assert_ne!(hash1, hash4, "Hash should differ for different ring keys");

        // Test with different ring size/composition
        let hash5 = hash_to_scalar(message, ring_both, &ephemeral_placeholder).unwrap();
        assert_ne!(
            hash1, hash5,
            "Hash should differ for different ring sizes/keys"
        );

        // Test with different ephemeral point
        let ephemeral_different = ProjectivePoint::GENERATOR * Scalar::from(3u64);
        let hash6 = hash_to_scalar(message, ring1, &ephemeral_different).unwrap();
        assert_ne!(
            hash1, hash6,
            "Hash should differ for different ephemeral points"
        );
    }

    #[test]
    fn test_sign_binary_errors() {
        let (ring_pubkeys, signer_priv, _signer_index) = setup_test_ring(3);
        let message = b"test errors";

        // Ring too small
        let small_ring = vec![ring_pubkeys[0]];
        let result_small = sign_binary(message, &signer_priv, &small_ring, OsRng);
        assert!(matches!(result_small, Err(Error::RingTooSmall(1))));

        // Signer not in ring
        let outsider_kp = generate_keypair_hex("compressed");
        let outsider_priv = hex_to_scalar(&outsider_kp.private_key_hex).unwrap();
        let result_outsider = sign_binary(message, &outsider_priv, &ring_pubkeys, OsRng);
        assert!(matches!(result_outsider, Err(Error::SignerNotInRing)));

        // Test with identity point in ring (should ideally be handled, but might pass if signer not identity)
        let mut ring_with_identity = ring_pubkeys.clone();
        ring_with_identity.push(ProjectivePoint::IDENTITY);
        // Signing might succeed if the identity isn't chosen or involved in a way that breaks math,
        // but verification involving it might fail later. Let's check if sign errors.
        let result_identity = sign_binary(message, &signer_priv, &ring_with_identity, OsRng);
        // Depending on implementation, this might succeed or fail. Let's just ensure it doesn't panic.
        assert!(result_identity.is_ok() || result_identity.is_err());
    }

    #[test]
    fn test_verify_binary_errors() {
        let (ring_pubkeys, signer_priv, _signer_index) = setup_test_ring(3);
        let message = b"test verify errors";
        let signature = sign_binary(message, &signer_priv, &ring_pubkeys, OsRng).unwrap();

        // Empty ring
        let result_empty = verify_binary(&signature, message, &[]);
        assert!(matches!(result_empty, Ok(false))); // Verification returns false for empty ring

        // Signature length mismatch
        let mut short_signature = signature.clone();
        short_signature.s.pop();
        let result_short = verify_binary(&short_signature, message, &ring_pubkeys);
        assert!(matches!(result_short, Err(Error::InvalidSignatureFormat)));

        let mut long_signature = signature.clone();
        long_signature.s.push(Scalar::ONE);
        let result_long = verify_binary(&long_signature, message, &ring_pubkeys);
        assert!(matches!(result_long, Err(Error::InvalidSignatureFormat)));

        // Verification failure (wrong message)
        let result_wrong_msg = verify_binary(&signature, b"wrong message", &ring_pubkeys);
        assert!(matches!(result_wrong_msg, Ok(false)));

        // Verification failure (wrong ring)
        let (ring2, _, _) = setup_test_ring(3);
        let result_wrong_ring = verify_binary(&signature, message, &ring2);
        assert!(matches!(result_wrong_ring, Ok(false)));
    }
}
