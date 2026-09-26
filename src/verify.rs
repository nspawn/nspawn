//! Image signatures: the cosign/Sigstore bundles a registry keeps as referrers of an
//! image, checked at pull against a policy per registry. The hub's policy is built in:
//! its images are signed by their build workflow, once with the project's key and once
//! keyless, and one of the two has to verify.

use std::fmt;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use base64::Engine as _;
use oci_client::Reference;
use sha2::{Digest as _, Sha256};
use sigstore_verify::trust_root::{TrustedRoot, SIGSTORE_PRODUCTION_TRUSTED_ROOT};
use sigstore_verify::types::bundle::VerificationMaterialContent;
use sigstore_verify::types::{
    Artifact, Bundle, DerPublicKey, Sha256Hash, SignatureContent, Statement,
};
use sigstore_verify::{PublicKeyVerificationPolicy, VerificationPolicy, Verifier};

use crate::api::{line, note, Report};
use crate::config::{Config, DEFAULT_REGISTRY};
use crate::hub::{short_digest, Hub, Resolved};

/// The key the hub's build workflow signs with: cosign.pub of nspawn/mkosi-definitions.
pub const HUB_KEY_PEM: &str = include_str!("verify/hub.nspawn.org.pub");
/// The workflow behind the hub's keyless signatures, and the issuer of its token.
pub const HUB_IDENTITY: &str =
    "https://github.com/nspawn/mkosi-definitions/.github/workflows/mkosi.yml@refs/heads/master";
pub const HUB_ISSUER: &str = "https://token.actions.githubusercontent.com";
/// A Sigstore bundle among an image's referrers.
pub const BUNDLE_ARTIFACT_TYPE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";
/// What cosign sign attests: the subject and nothing else.
pub const COSIGN_SIGN_PREDICATE: &str = "https://sigstore.dev/cosign/sign/v1";
/// A bundle is a few KB; nothing bigger is read into memory.
pub const BUNDLE_MAX_SIZE: u64 = 1 << 20;

/// A public key a registry's images may be signed with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyKey {
    /// Where it comes from, for messages: "built-in" or the file's path.
    pub label: String,
    pub pem: String,
}

/// A keyless signer: the certificate's identity and the issuer of the token behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub identity: String,
    pub issuer: String,
}

/// What a registry's images must carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub registry: String,
    pub keys: Vec<PolicyKey>,
    pub identity: Option<Identity>,
    /// Without a verifying signature the pull fails (true) or goes on with a note (false).
    pub required: bool,
    /// The bundle's transparency log entry must verify (true) or is left unchecked (false).
    pub rekor: bool,
    /// Another Sigstore deployment's trusted root; the public-good one otherwise.
    pub trusted_root: Option<PathBuf>,
}

impl Policy {
    /// The hub's: the project's key or the workflow's identity, a signature required,
    /// its log entry checked.
    pub fn hub() -> Self {
        Policy {
            registry: DEFAULT_REGISTRY.to_string(),
            keys: vec![PolicyKey {
                label: "built-in".to_string(),
                pem: HUB_KEY_PEM.to_string(),
            }],
            identity: Some(Identity {
                identity: HUB_IDENTITY.to_string(),
                issuer: HUB_ISSUER.to_string(),
            }),
            required: true,
            rekor: true,
            trusted_root: None,
        }
    }

    /// The policy for a registry: the built-in one for the hub, none for the rest.
    pub fn for_registry(_config: &Config, registry: &str) -> Result<Option<Policy>> {
        if crate::auth::canonical(registry) == DEFAULT_REGISTRY {
            return Ok(Some(Policy::hub()));
        }
        Ok(None)
    }
}

/// Who signed, as the verification found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signer {
    /// The hint cosign writes for the key: base64 of the sha256 of its DER form.
    Key {
        hint: String,
    },
    Keyless {
        identity: String,
        issuer: String,
    },
}

impl fmt::Display for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Signer::Key { hint } => write!(f, "key {hint}"),
            Signer::Keyless { identity, .. } => write!(f, "keyless {identity}"),
        }
    }
}

impl Signer {
    /// For a line of output: the hint cut short, the identity whole (it says who).
    pub fn short(&self) -> String {
        match self {
            Signer::Key { hint } => format!("key {}", short_hint(hint)),
            Signer::Keyless { identity, .. } => format!("keyless {identity}"),
        }
    }
}

fn short_hint(hint: &str) -> String {
    hint.chars().take(12).collect()
}

/// A signature that verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub signer: Signer,
    /// When it was made, in unix seconds, as the transparency log or a timestamp
    /// authority vouched for it.
    pub time: Option<u64>,
    /// The signature manifest it came from, shortened.
    pub bundle: String,
}

/// A bundle as fetched: the signature manifest it hung from, and its JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub manifest_digest: String,
    pub json: String,
}

/// What a verified pull remembers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// Every signer that verified, "key <hint>, keyless <identity>".
    pub signed_by: String,
    /// The earliest vouched-for signing time, when there is one.
    pub signed_at: Option<u64>,
}

impl Signed {
    pub fn from_verified(verified: &[Verified]) -> Self {
        Signed {
            signed_by: signed_by(verified),
            signed_at: verified.iter().filter_map(|v| v.time).min(),
        }
    }
}

pub fn signed_by(verified: &[Verified]) -> String {
    verified
        .iter()
        .map(|v| v.signer.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The line pull prints once an image's signature verified.
pub fn verified_line(image: &str, verified: &[Verified]) -> String {
    format!(
        "{image}: signature verified ({})",
        verified
            .iter()
            .map(|v| v.signer.short())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The signatures of an image against the policy of its registry, before anything of it
/// is downloaded: the referrers of the index the reference named, when it named one,
/// else of the manifest. What is remembered when one verifies; None when the policy does
/// not require a signature and there is none.
pub async fn check(
    hub: &Hub,
    oci: &Reference,
    image: &str,
    resolved: &Resolved,
    policy: &Policy,
    report: Report<'_>,
) -> Result<Option<Signed>> {
    let mut found = None;
    for subject in resolved
        .index_digest
        .iter()
        .chain(std::iter::once(&resolved.digest))
    {
        let bundles = match hub.signature_bundles(oci, subject).await {
            Ok(bundles) => bundles,
            Err(e) if policy.required => return Err(e),
            Err(e) => {
                note(
                    report,
                    format!(
                        "warning: {e:#}; the policy for {} does not require a signature",
                        policy.registry
                    ),
                );
                return Ok(None);
            }
        };
        if !bundles.is_empty() {
            found = Some((subject, bundles));
            break;
        }
    }
    let Some((subject, bundles)) = found else {
        if policy.required {
            bail!(
                "{image} ({}) carries no signature on {}; the policy requires one (--no-verify pulls it anyway)",
                short_digest(&resolved.digest),
                policy.registry
            );
        }
        note(
            report,
            format!(
                "note: {image} carries no signature; the policy for {} does not require one",
                policy.registry
            ),
        );
        return Ok(None);
    };
    let verified = verify_bundles(policy, subject, &bundles).with_context(|| image.to_string())?;
    line(report, verified_line(image, &verified));
    Ok(Some(Signed::from_verified(&verified)))
}

/// The hint cosign writes into a bundle signed with `key`.
pub fn key_hint(key: &DerPublicKey) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(key.as_bytes()))
}

/// A key of the policy, parsed, with the hint a bundle signed with it carries.
struct LoadedKey {
    label: String,
    key: DerPublicKey,
    hint: String,
}

/// Every bundle against the policy for the image whose manifest digest is `digest`:
/// the ones that verify (never empty), or why none did, bundle by bundle.
pub fn verify_bundles(
    policy: &Policy,
    digest: &str,
    bundles: &[Candidate],
) -> Result<Vec<Verified>> {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    let artifact = Sha256Hash::from_hex(hex).with_context(|| format!("digest {digest}"))?;
    let root = match &policy.trusted_root {
        Some(path) => TrustedRoot::from_file(path)
            .with_context(|| format!("reading the trusted root {}", path.display()))?,
        None => TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT)
            .context("the embedded Sigstore trusted root")?,
    };
    let verifier = Verifier::new(&root).context("preparing the Sigstore trusted root")?;
    let keys = policy
        .keys
        .iter()
        .map(|k| {
            let key = DerPublicKey::from_pem(&k.pem).with_context(|| {
                format!(
                    "public key {} of the policy for {}",
                    k.label, policy.registry
                )
            })?;
            Ok(LoadedKey {
                label: k.label.clone(),
                hint: key_hint(&key),
                key,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut verified = Vec::new();
    let mut reasons = Vec::new();
    for candidate in bundles {
        match verify_one(&verifier, policy, &keys, artifact, candidate) {
            Ok(v) => verified.push(v),
            Err(reason) => reasons.push(format!(
                "{}: {reason}",
                short_digest(&candidate.manifest_digest)
            )),
        }
    }
    if verified.is_empty() {
        bail!(
            "no signature verifies under the policy for {}: {}",
            policy.registry,
            reasons.join("; ")
        );
    }
    // Keys first, whatever order the registry listed the bundles in: the line and the
    // record read the same on every host.
    verified.sort_by_key(|v| matches!(v.signer, Signer::Keyless { .. }));
    Ok(verified)
}

/// One bundle; the reason of a failure is one line, starting with the kind of signature.
fn verify_one(
    verifier: &Verifier,
    policy: &Policy,
    keys: &[LoadedKey],
    artifact: Sha256Hash,
    candidate: &Candidate,
) -> std::result::Result<Verified, String> {
    let bundle =
        Bundle::from_json(&candidate.json).map_err(|e| format!("unreadable bundle: {e}"))?;
    let SignatureContent::DsseEnvelope(envelope) = &bundle.content else {
        return Err("not a signature of cosign 3 (a bare message signature)".to_string());
    };
    let statement: Statement = serde_json::from_slice(envelope.payload.as_bytes())
        .map_err(|e| format!("unreadable statement: {e}"))?;
    if statement.predicate_type != COSIGN_SIGN_PREDICATE {
        return Err(format!(
            "not a cosign signature (an attestation of {})",
            statement.predicate_type
        ));
    }
    let bundle_name = short_digest(&candidate.manifest_digest);
    match &bundle.verification_material.content {
        VerificationMaterialContent::PublicKey { hint } => {
            if keys.is_empty() {
                return Err(format!("key {}: the policy names no key", short_hint(hint)));
            }
            let mut pk_policy = PublicKeyVerificationPolicy::default();
            if !policy.rekor {
                pk_policy = pk_policy.skip_tlog_unsafe();
            }
            // The key the hint names goes first: its failure is the one worth reading.
            let (named, others): (Vec<&LoadedKey>, Vec<&LoadedKey>) =
                keys.iter().partition(|k| k.hint == *hint);
            let mut last = String::new();
            for key in named.iter().chain(others.iter()) {
                match verifier.verify_with_key(
                    Artifact::from_digest(artifact),
                    &bundle,
                    &key.key,
                    &pk_policy,
                ) {
                    Ok(result) => {
                        return Ok(Verified {
                            signer: Signer::Key { hint: hint.clone() },
                            time: result
                                .integrated_time()
                                .map(|t| t.as_second().max(0) as u64),
                            bundle: bundle_name,
                        });
                    }
                    Err(e) => last = format!("{} ({e})", key.label),
                }
            }
            Err(if named.is_empty() {
                format!(
                    "key {}: its hint matches none of the configured keys; tried {last}",
                    short_hint(hint)
                )
            } else {
                format!("key {}: {last}", short_hint(hint))
            })
        }
        VerificationMaterialContent::Certificate(_) => {
            let Some(identity) = &policy.identity else {
                return Err("keyless: the policy names no identity".to_string());
            };
            let mut cert_policy = VerificationPolicy::new(&identity.identity, &identity.issuer);
            if !policy.rekor {
                cert_policy = cert_policy.skip_tlog_unsafe();
            }
            match verifier.verify(Artifact::from_digest(artifact), &bundle, &cert_policy) {
                Ok(result) => {
                    let times = result
                        .verified_timestamps()
                        .iter()
                        .map(|t| t.as_second())
                        .chain(result.integrated_time().map(|t| t.as_second()))
                        .min();
                    Ok(Verified {
                        signer: Signer::Keyless {
                            identity: result.identity().unwrap_or(&identity.identity).to_string(),
                            issuer: result.issuer().unwrap_or(&identity.issuer).to_string(),
                        },
                        time: times.map(|t| t.max(0) as u64),
                        bundle: bundle_name,
                    })
                }
                Err(e) => Err(format!("keyless: {e}")),
            }
        }
        VerificationMaterialContent::X509CertificateChain { .. } => {
            Err("a bundle of the old format (v0.1/v0.2)".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KALI_DIGEST: &str =
        "sha256:24a0bba642e3ed90b94428a0bf8e9cfaac49d46f4844fc1801a0267a9b9d693a";
    const KEY_BUNDLE: &str = include_str!("../tests/fixtures/signatures/key.bundle.json");
    const KEYLESS_BUNDLE: &str = include_str!("../tests/fixtures/signatures/keyless.bundle.json");
    const OTHER_KEY: &str = include_str!("../tests/fixtures/signatures/other.pub");
    const HUB_HINT: &str = "6wiWMtJZCUkV05p2cDF3DrfY9Q1YsYRMLaOYI/PY7Lc=";

    /// As the hub lists them: the keyless one first.
    fn candidates() -> Vec<Candidate> {
        vec![
            Candidate {
                manifest_digest:
                    "sha256:7b5b8bc25c8392e980edcdefba7fcb85805ad18301b006b68183cf540ba30ed3"
                        .to_string(),
                json: KEYLESS_BUNDLE.to_string(),
            },
            Candidate {
                manifest_digest:
                    "sha256:aafdb47c55dcf12192e2818c588603a3c7285809d32a32eedda045a639372d3c"
                        .to_string(),
                json: KEY_BUNDLE.to_string(),
            },
        ]
    }

    #[test]
    fn the_hub_key_carries_the_hint_cosign_writes() {
        let key = DerPublicKey::from_pem(HUB_KEY_PEM).unwrap();
        assert_eq!(key_hint(&key), HUB_HINT);
        assert_ne!(
            key_hint(&DerPublicKey::from_pem(OTHER_KEY).unwrap()),
            HUB_HINT
        );
    }

    #[test]
    fn both_hub_signatures_verify_under_the_built_in_policy() {
        let verified = verify_bundles(&Policy::hub(), KALI_DIGEST, &candidates()).unwrap();
        assert_eq!(verified.len(), 2);
        assert_eq!(
            verified[0].signer,
            Signer::Key {
                hint: HUB_HINT.to_string()
            }
        );
        assert_eq!(
            verified[1].signer,
            Signer::Keyless {
                identity: HUB_IDENTITY.to_string(),
                issuer: HUB_ISSUER.to_string()
            }
        );
        for v in &verified {
            assert!(v.time.is_some_and(|t| t > 1_790_000_000), "{v:?}");
        }
        let signed = Signed::from_verified(&verified);
        assert_eq!(
            signed.signed_by,
            format!("key {HUB_HINT}, keyless {HUB_IDENTITY}")
        );
        assert_eq!(
            signed.signed_at,
            verified.iter().filter_map(|v| v.time).min()
        );
        assert_eq!(
            verified_line("hub.nspawn.org/kali:rolling", &verified),
            format!("hub.nspawn.org/kali:rolling: signature verified (key 6wiWMtJZCUkV, keyless {HUB_IDENTITY})")
        );
    }

    #[test]
    fn the_log_entry_can_be_left_unchecked() {
        let policy = Policy {
            rekor: false,
            ..Policy::hub()
        };
        assert_eq!(
            verify_bundles(&policy, KALI_DIGEST, &candidates())
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn what_the_policy_names_is_what_verifies() {
        let keys_only = Policy {
            identity: None,
            ..Policy::hub()
        };
        let verified = verify_bundles(&keys_only, KALI_DIGEST, &candidates()).unwrap();
        assert_eq!(
            verified.len(),
            1,
            "the keyless bundle is not for this policy"
        );
        assert!(matches!(verified[0].signer, Signer::Key { .. }));
        let identity_only = Policy {
            keys: Vec::new(),
            ..Policy::hub()
        };
        let verified = verify_bundles(&identity_only, KALI_DIGEST, &candidates()).unwrap();
        assert_eq!(verified.len(), 1);
        assert!(matches!(verified[0].signer, Signer::Keyless { .. }));
    }

    #[test]
    fn another_digest_key_or_identity_fails_with_every_reason() {
        let other = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let e = verify_bundles(&Policy::hub(), other, &candidates())
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("no signature verifies under the policy for hub.nspawn.org: 7b5b8bc25c83: keyless: "), "{e}");
        assert!(
            e.contains("; aafdb47c55dc: key 6wiWMtJZCUkV: built-in ("),
            "{e}"
        );

        let wrong_key = Policy {
            keys: vec![PolicyKey {
                label: "/etc/nspawn/other.pub".to_string(),
                pem: OTHER_KEY.to_string(),
            }],
            identity: None,
            ..Policy::hub()
        };
        let e = verify_bundles(&wrong_key, KALI_DIGEST, &candidates())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("its hint matches none of the configured keys; tried /etc/nspawn/other.pub"),
            "{e}"
        );
        assert!(e.contains("keyless: the policy names no identity"), "{e}");

        let wrong_identity = Policy {
            keys: Vec::new(),
            identity: Some(Identity {
                identity: "https://github.com/someone/else/.github/workflows/x.yml@refs/heads/main"
                    .to_string(),
                issuer: HUB_ISSUER.to_string(),
            }),
            ..Policy::hub()
        };
        let e = verify_bundles(&wrong_identity, KALI_DIGEST, &candidates())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("key 6wiWMtJZCUkV: the policy names no key"),
            "{e}"
        );
        assert!(e.contains("keyless: "), "{e}");
        assert!(e.to_lowercase().contains("identity"), "{e}");
    }

    #[test]
    fn what_is_no_cosign_signature_is_refused() {
        let json: serde_json::Value = serde_json::from_str(KEY_BUNDLE).unwrap();
        let with = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut v = json.clone();
            edit(&mut v);
            vec![Candidate {
                manifest_digest:
                    "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                        .to_string(),
                json: v.to_string(),
            }]
        };
        let reason = |candidates: Vec<Candidate>| {
            verify_bundles(&Policy::hub(), KALI_DIGEST, &candidates)
                .unwrap_err()
                .to_string()
        };
        // An attestation of something else, signed by the right key.
        let attestation = with(&|v| {
            let payload = base64::engine::general_purpose::STANDARD
                .decode(v["dsseEnvelope"]["payload"].as_str().unwrap())
                .unwrap();
            let text = String::from_utf8(payload)
                .unwrap()
                .replace(COSIGN_SIGN_PREDICATE, "https://slsa.dev/provenance/v1");
            v["dsseEnvelope"]["payload"] = base64::engine::general_purpose::STANDARD
                .encode(text)
                .into();
        });
        assert!(reason(attestation)
            .contains("not a cosign signature (an attestation of https://slsa.dev/provenance/v1)"));
        // A bundle without log entry or timestamp: nothing vouches for when it was made.
        let bare = with(&|v| {
            v["verificationMaterial"]["tlogEntries"] = serde_json::json!([]);
            v["verificationMaterial"]
                .as_object_mut()
                .unwrap()
                .remove("timestampVerificationData");
        });
        let policy = Policy {
            rekor: false,
            ..Policy::hub()
        };
        let e = verify_bundles(&policy, KALI_DIGEST, &bare)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("111111111111: key 6wiWMtJZCUkV: built-in ("),
            "{e}"
        );
        // A tampered signature.
        let tampered = with(&|v| {
            let sig = v["dsseEnvelope"]["signatures"][0]["sig"]
                .as_str()
                .unwrap()
                .to_string();
            let mut bytes = base64::engine::general_purpose::STANDARD
                .decode(&sig)
                .unwrap();
            bytes[10] ^= 0x01;
            v["dsseEnvelope"]["signatures"][0]["sig"] = base64::engine::general_purpose::STANDARD
                .encode(bytes)
                .into();
        });
        assert!(reason(tampered).contains("key 6wiWMtJZCUkV: built-in ("));
        // Not JSON at all.
        let e = reason(vec![Candidate {
            manifest_digest:
                "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                    .to_string(),
            json: "not json".to_string(),
        }]);
        assert!(e.contains("222222222222: unreadable bundle: "), "{e}");
    }

    #[test]
    fn the_hub_has_a_policy_and_the_rest_none() {
        let config = Config::merge(Default::default(), None, None).unwrap();
        assert_eq!(
            Policy::for_registry(&config, "hub.nspawn.org").unwrap(),
            Some(Policy::hub())
        );
        assert_eq!(
            Policy::for_registry(&config, "HUB.nspawn.org").unwrap(),
            Some(Policy::hub()),
            "as the registry name is spelled"
        );
        assert_eq!(Policy::for_registry(&config, "docker.io").unwrap(), None);
        assert_eq!(
            Policy::for_registry(&config, "hub.nspawn.test:8443").unwrap(),
            None
        );
    }
}
