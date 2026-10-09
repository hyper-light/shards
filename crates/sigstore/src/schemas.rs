//! The Sigstore messages protojson reads, as protobuf-specs v0.5.1 declares them
//! (gen/pb-go: bundle/v1, common/v1, rekor/v1, trustroot/v1, dsse): each field's JSON and
//! proto names, number, kind, and oneof.

use crate::proto::{Kind, Schema, field};

/// dev.sigstore.common.v1.HashAlgorithm.
pub const HASH_ALGORITHM: &[(&str, i32)] = &[
    ("HASH_ALGORITHM_UNSPECIFIED", 0),
    ("SHA2_256", 1),
    ("SHA2_384", 2),
    ("SHA2_512", 3),
    ("SHA3_256", 4),
    ("SHA3_384", 5),
];

/// dev.sigstore.common.v1.PublicKeyDetails.
pub const PUBLIC_KEY_DETAILS: &[(&str, i32)] = &[
    ("PUBLIC_KEY_DETAILS_UNSPECIFIED", 0),
    ("PKCS1_RSA_PKCS1V5", 1),
    ("PKCS1_RSA_PSS", 2),
    ("PKIX_RSA_PKCS1V5", 3),
    ("PKIX_RSA_PSS", 4),
    ("PKIX_RSA_PKCS1V15_2048_SHA256", 9),
    ("PKIX_RSA_PKCS1V15_3072_SHA256", 10),
    ("PKIX_RSA_PKCS1V15_4096_SHA256", 11),
    ("PKIX_RSA_PSS_2048_SHA256", 16),
    ("PKIX_RSA_PSS_3072_SHA256", 17),
    ("PKIX_RSA_PSS_4096_SHA256", 18),
    ("PKIX_ECDSA_P256_HMAC_SHA_256", 6),
    ("PKIX_ECDSA_P256_SHA_256", 5),
    ("PKIX_ECDSA_P384_SHA_384", 12),
    ("PKIX_ECDSA_P521_SHA_512", 13),
    ("PKIX_ED25519", 7),
    ("PKIX_ED25519_PH", 8),
    ("PKIX_ECDSA_P384_SHA_256", 19),
    ("PKIX_ECDSA_P521_SHA_256", 20),
    ("LMS_SHA256", 14),
    ("LMOTS_SHA256", 15),
    ("ML_DSA_44", 23),
    ("ML_DSA_65", 21),
    ("ML_DSA_87", 22),
];

/// An enum value's name as protobuf-go's String prints it: its name, or its number.
pub fn enum_name(names: &[(&str, i32)], v: i32) -> String {
    names
        .iter()
        .find(|(_, n)| *n == v)
        .map_or_else(|| v.to_string(), |(name, _)| (*name).to_string())
}

pub static LOG_ID: Schema = Schema {
    name: "dev.sigstore.common.v1.LogId",
    fields: &[field("keyId", "key_id", 1, Kind::Bytes)],
    oneofs: &[],
};

pub static X509_CERTIFICATE: Schema = Schema {
    name: "dev.sigstore.common.v1.X509Certificate",
    fields: &[field("rawBytes", "raw_bytes", 1, Kind::Bytes)],
    oneofs: &[],
};

pub static X509_CERTIFICATE_CHAIN: Schema = Schema {
    name: "dev.sigstore.common.v1.X509CertificateChain",
    fields: &[field(
        "certificates",
        "certificates",
        1,
        Kind::Message(&X509_CERTIFICATE),
    )
    .repeated()],
    oneofs: &[],
};

pub static PUBLIC_KEY_IDENTIFIER: Schema = Schema {
    name: "dev.sigstore.common.v1.PublicKeyIdentifier",
    fields: &[field("hint", "hint", 1, Kind::Str)],
    oneofs: &[],
};

pub static RFC3161_SIGNED_TIMESTAMP: Schema = Schema {
    name: "dev.sigstore.common.v1.RFC3161SignedTimestamp",
    fields: &[field("signedTimestamp", "signed_timestamp", 1, Kind::Bytes)],
    oneofs: &[],
};

pub static HASH_OUTPUT: Schema = Schema {
    name: "dev.sigstore.common.v1.HashOutput",
    fields: &[
        field("algorithm", "algorithm", 1, Kind::Enum(HASH_ALGORITHM)),
        field("digest", "digest", 2, Kind::Bytes),
    ],
    oneofs: &[],
};

pub static MESSAGE_SIGNATURE: Schema = Schema {
    name: "dev.sigstore.common.v1.MessageSignature",
    fields: &[
        field("messageDigest", "message_digest", 1, Kind::Message(&HASH_OUTPUT)),
        field("signature", "signature", 2, Kind::Bytes),
    ],
    oneofs: &[],
};

pub static TIME_RANGE: Schema = Schema {
    name: "dev.sigstore.common.v1.TimeRange",
    fields: &[
        field("start", "start", 1, Kind::Timestamp),
        field("end", "end", 2, Kind::Timestamp).optional(),
    ],
    oneofs: &[],
};

pub static PUBLIC_KEY: Schema = Schema {
    name: "dev.sigstore.common.v1.PublicKey",
    fields: &[
        field("rawBytes", "raw_bytes", 1, Kind::Bytes).optional(),
        field("keyDetails", "key_details", 2, Kind::Enum(PUBLIC_KEY_DETAILS)),
        field("validFor", "valid_for", 3, Kind::Message(&TIME_RANGE)).optional(),
    ],
    oneofs: &[],
};

pub static DISTINGUISHED_NAME: Schema = Schema {
    name: "dev.sigstore.common.v1.DistinguishedName",
    fields: &[
        field("organization", "organization", 1, Kind::Str),
        field("commonName", "common_name", 2, Kind::Str),
    ],
    oneofs: &[],
};

pub static KIND_VERSION: Schema = Schema {
    name: "dev.sigstore.rekor.v1.KindVersion",
    fields: &[
        field("kind", "kind", 1, Kind::Str),
        field("version", "version", 2, Kind::Str),
    ],
    oneofs: &[],
};

pub static CHECKPOINT: Schema = Schema {
    name: "dev.sigstore.rekor.v1.Checkpoint",
    fields: &[field("envelope", "envelope", 1, Kind::Str)],
    oneofs: &[],
};

pub static INCLUSION_PROOF: Schema = Schema {
    name: "dev.sigstore.rekor.v1.InclusionProof",
    fields: &[
        field("logIndex", "log_index", 1, Kind::Int64),
        field("rootHash", "root_hash", 2, Kind::Bytes),
        field("treeSize", "tree_size", 3, Kind::Int64),
        field("hashes", "hashes", 4, Kind::Bytes).repeated(),
        field("checkpoint", "checkpoint", 5, Kind::Message(&CHECKPOINT)),
    ],
    oneofs: &[],
};

pub static INCLUSION_PROMISE: Schema = Schema {
    name: "dev.sigstore.rekor.v1.InclusionPromise",
    fields: &[field(
        "signedEntryTimestamp",
        "signed_entry_timestamp",
        1,
        Kind::Bytes,
    )],
    oneofs: &[],
};

pub static TRANSPARENCY_LOG_ENTRY: Schema = Schema {
    name: "dev.sigstore.rekor.v1.TransparencyLogEntry",
    fields: &[
        field("logIndex", "log_index", 1, Kind::Int64),
        field("logId", "log_id", 2, Kind::Message(&LOG_ID)),
        field("kindVersion", "kind_version", 3, Kind::Message(&KIND_VERSION)),
        field("integratedTime", "integrated_time", 4, Kind::Int64),
        field(
            "inclusionPromise",
            "inclusion_promise",
            5,
            Kind::Message(&INCLUSION_PROMISE),
        ),
        field(
            "inclusionProof",
            "inclusion_proof",
            6,
            Kind::Message(&INCLUSION_PROOF),
        ),
        field("canonicalizedBody", "canonicalized_body", 7, Kind::Bytes),
    ],
    oneofs: &[],
};

pub static TIMESTAMP_VERIFICATION_DATA: Schema = Schema {
    name: "dev.sigstore.bundle.v1.TimestampVerificationData",
    fields: &[field(
        "rfc3161Timestamps",
        "rfc3161_timestamps",
        1,
        Kind::Message(&RFC3161_SIGNED_TIMESTAMP),
    )
    .repeated()],
    oneofs: &[],
};

pub static VERIFICATION_MATERIAL: Schema = Schema {
    name: "dev.sigstore.bundle.v1.VerificationMaterial",
    fields: &[
        field(
            "publicKey",
            "public_key",
            1,
            Kind::Message(&PUBLIC_KEY_IDENTIFIER),
        )
        .oneof(0),
        field(
            "x509CertificateChain",
            "x509_certificate_chain",
            2,
            Kind::Message(&X509_CERTIFICATE_CHAIN),
        )
        .oneof(0),
        field("certificate", "certificate", 5, Kind::Message(&X509_CERTIFICATE)).oneof(0),
        field(
            "tlogEntries",
            "tlog_entries",
            3,
            Kind::Message(&TRANSPARENCY_LOG_ENTRY),
        )
        .repeated(),
        field(
            "timestampVerificationData",
            "timestamp_verification_data",
            4,
            Kind::Message(&TIMESTAMP_VERIFICATION_DATA),
        ),
    ],
    oneofs: &["dev.sigstore.bundle.v1.VerificationMaterial.content"],
};

pub static DSSE_SIGNATURE: Schema = Schema {
    name: "io.intoto.Signature",
    fields: &[
        field("sig", "sig", 1, Kind::Bytes),
        field("keyid", "keyid", 2, Kind::Str),
    ],
    oneofs: &[],
};

pub static DSSE_ENVELOPE: Schema = Schema {
    name: "io.intoto.Envelope",
    fields: &[
        field("payload", "payload", 1, Kind::Bytes),
        field("payloadType", "payloadType", 2, Kind::Str),
        field("signatures", "signatures", 3, Kind::Message(&DSSE_SIGNATURE)).repeated(),
    ],
    oneofs: &[],
};

pub static BUNDLE: Schema = Schema {
    name: "dev.sigstore.bundle.v1.Bundle",
    fields: &[
        field("mediaType", "media_type", 1, Kind::Str),
        field(
            "verificationMaterial",
            "verification_material",
            2,
            Kind::Message(&VERIFICATION_MATERIAL),
        ),
        field(
            "messageSignature",
            "message_signature",
            3,
            Kind::Message(&MESSAGE_SIGNATURE),
        )
        .oneof(0),
        field("dsseEnvelope", "dsse_envelope", 4, Kind::Message(&DSSE_ENVELOPE)).oneof(0),
    ],
    oneofs: &["dev.sigstore.bundle.v1.Bundle.content"],
};

pub static TRANSPARENCY_LOG_INSTANCE: Schema = Schema {
    name: "dev.sigstore.trustroot.v1.TransparencyLogInstance",
    fields: &[
        field("baseUrl", "base_url", 1, Kind::Str),
        field("hashAlgorithm", "hash_algorithm", 2, Kind::Enum(HASH_ALGORITHM)),
        field("publicKey", "public_key", 3, Kind::Message(&PUBLIC_KEY)),
        field("logId", "log_id", 4, Kind::Message(&LOG_ID)),
        field("checkpointKeyId", "checkpoint_key_id", 5, Kind::Message(&LOG_ID)),
        field("operator", "operator", 6, Kind::Str),
    ],
    oneofs: &[],
};

pub static CERTIFICATE_AUTHORITY: Schema = Schema {
    name: "dev.sigstore.trustroot.v1.CertificateAuthority",
    fields: &[
        field("subject", "subject", 1, Kind::Message(&DISTINGUISHED_NAME)),
        field("uri", "uri", 2, Kind::Str),
        field(
            "certChain",
            "cert_chain",
            3,
            Kind::Message(&X509_CERTIFICATE_CHAIN),
        ),
        field("validFor", "valid_for", 4, Kind::Message(&TIME_RANGE)),
        field("operator", "operator", 5, Kind::Str),
    ],
    oneofs: &[],
};

pub static TRUSTED_ROOT: Schema = Schema {
    name: "dev.sigstore.trustroot.v1.TrustedRoot",
    fields: &[
        field("mediaType", "media_type", 1, Kind::Str),
        field("tlogs", "tlogs", 2, Kind::Message(&TRANSPARENCY_LOG_INSTANCE)).repeated(),
        field(
            "certificateAuthorities",
            "certificate_authorities",
            3,
            Kind::Message(&CERTIFICATE_AUTHORITY),
        )
        .repeated(),
        field("ctlogs", "ctlogs", 4, Kind::Message(&TRANSPARENCY_LOG_INSTANCE)).repeated(),
        field(
            "timestampAuthorities",
            "timestamp_authorities",
            5,
            Kind::Message(&CERTIFICATE_AUTHORITY),
        )
        .repeated(),
    ],
    oneofs: &[],
};

pub static RESOURCE_DESCRIPTOR: Schema = Schema {
    name: "in_toto_attestation.v1.ResourceDescriptor",
    fields: &[
        field("name", "name", 1, Kind::Str),
        field("uri", "uri", 2, Kind::Str),
        field("digest", "digest", 3, Kind::StringMap),
        field("content", "content", 4, Kind::Bytes),
        field("downloadLocation", "download_location", 5, Kind::Str),
        field("mediaType", "media_type", 6, Kind::Str),
        field("annotations", "annotations", 7, Kind::Struct),
    ],
    oneofs: &[],
};

/// in_toto_attestation.v1.Statement (github.com/in-toto/attestation/go/v1).
pub static STATEMENT: Schema = Schema {
    name: "in_toto_attestation.v1.Statement",
    fields: &[
        field("_type", "type", 1, Kind::Str),
        field("subject", "subject", 2, Kind::Message(&RESOURCE_DESCRIPTOR)).repeated(),
        field("predicateType", "predicate_type", 3, Kind::Str),
        field("predicate", "predicate", 4, Kind::Struct),
    ],
    oneofs: &[],
};
