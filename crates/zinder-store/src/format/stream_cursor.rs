//! Stream cursor byte contract.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;
use zinder_core::{BlockHash, BlockHeight, Network};

/// Fixed byte length of a [`StreamCursorTokenV1`].
pub const STREAM_CURSOR_TOKEN_V1_LEN: usize = 82;

const STREAM_CURSOR_SCHEMA_VERSION: u8 = 1;
const STREAM_FAMILY_MASK: u8 = 0x0f;
const STREAM_RESERVED_FLAGS_MASK: u8 = 0xf0;
const CURSOR_BODY_LEN: usize = 50;
const AUTH_TAG_LEN: usize = 32;

type HmacSha256 = Hmac<Sha256>;

/// Chain-event stream family encoded in the low nibble of a cursor flags byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChainEventStreamFamily {
    /// Tip stream: every committed chain event, including reorg events.
    Tip,
    /// Finalized stream: only finalized chain events; never reorg events.
    Finalized,
}

impl ChainEventStreamFamily {
    pub(crate) const fn flags(self) -> u8 {
        match self {
            Self::Tip => 0x0,
            Self::Finalized => 0x1,
        }
    }

    const fn from_flags(flags: u8) -> Option<Self> {
        if flags & STREAM_RESERVED_FLAGS_MASK != 0 {
            return None;
        }

        match flags & STREAM_FAMILY_MASK {
            0x0 => Some(Self::Tip),
            0x1 => Some(Self::Finalized),
            _ => None,
        }
    }
}

/// Cursor payload decoded from a chain-event stream cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChainEventCursorPayload {
    pub(crate) family: ChainEventStreamFamily,
    pub(crate) event_sequence: u64,
    pub(crate) last_height: BlockHeight,
    pub(crate) last_hash: BlockHash,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChainEventCursorAnchor {
    pub(crate) height: BlockHeight,
    pub(crate) hash: BlockHash,
}

/// Fixed-layout cursor token for resumable streams.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StreamCursorTokenV1(Vec<u8>);

impl StreamCursorTokenV1 {
    pub(crate) fn chain_event(
        network: Network,
        family: ChainEventStreamFamily,
        event_sequence: u64,
        anchor: ChainEventCursorAnchor,
        cursor_auth_key: [u8; 32],
    ) -> Result<Self, StreamCursorError> {
        let mut cursor_bytes = Vec::with_capacity(STREAM_CURSOR_TOKEN_V1_LEN);
        cursor_bytes.push(STREAM_CURSOR_SCHEMA_VERSION);
        cursor_bytes.extend_from_slice(&network.id().to_be_bytes());
        cursor_bytes.extend_from_slice(&event_sequence.to_be_bytes());
        cursor_bytes.extend_from_slice(&anchor.height.value().to_be_bytes());
        cursor_bytes.extend_from_slice(&anchor.hash.as_bytes());
        cursor_bytes.push(family.flags());

        let auth_tag = compute_auth_tag(cursor_auth_key, &cursor_bytes)?;
        cursor_bytes.extend_from_slice(&auth_tag);

        Ok(Self(cursor_bytes))
    }

    pub(crate) fn decode_chain_event(
        &self,
        expected_network: Network,
        cursor_auth_key: [u8; 32],
    ) -> Result<ChainEventCursorPayload, StreamCursorError> {
        if self.0.len() != STREAM_CURSOR_TOKEN_V1_LEN {
            return Err(StreamCursorError::InvalidLength {
                byte_count: self.0.len(),
            });
        }

        if self.0[0] != STREAM_CURSOR_SCHEMA_VERSION {
            return Err(StreamCursorError::UnsupportedSchemaVersion { version: self.0[0] });
        }

        let network_id = read_u32_be(&self.0, 1)?;
        let network =
            Network::from_id(network_id).ok_or(StreamCursorError::UnknownNetwork { network_id })?;
        if network != expected_network {
            return Err(StreamCursorError::NetworkMismatch {
                expected: expected_network,
                actual: network,
            });
        }

        let flags = self.0[49];
        let family = ChainEventStreamFamily::from_flags(flags)
            .ok_or(StreamCursorError::StreamFamilyMismatch { flags })?;
        if !matches!(
            family,
            ChainEventStreamFamily::Tip | ChainEventStreamFamily::Finalized
        ) {
            return Err(StreamCursorError::StreamFamilyMismatch { flags });
        }

        verify_auth_tag(
            cursor_auth_key,
            &self.0[..CURSOR_BODY_LEN],
            &self.0[CURSOR_BODY_LEN..],
        )?;

        let hash_bytes = <[u8; 32]>::try_from(&self.0[17..49]).map_err(|_| {
            StreamCursorError::InvalidLength {
                byte_count: self.0.len(),
            }
        })?;

        Ok(ChainEventCursorPayload {
            family,
            event_sequence: read_u64_be(&self.0, 5)?,
            last_height: BlockHeight::new(read_u32_be(&self.0, 13)?),
            last_hash: BlockHash::from_bytes(hash_bytes),
        })
    }

    /// Creates a cursor token from encoded bytes supplied by a client.
    #[must_use]
    pub fn from_bytes(cursor_bytes: impl Into<Vec<u8>>) -> Self {
        Self(cursor_bytes.into())
    }

    /// Returns the encoded cursor bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Encodes this cursor token as unpadded base64url text.
    #[must_use]
    pub fn to_base64url(&self) -> String {
        BASE64_URL_SAFE_NO_PAD.encode(&self.0)
    }

    /// Decodes a cursor token from unpadded base64url text.
    pub fn from_base64url(cursor_text: &str) -> Result<Self, StreamCursorError> {
        BASE64_URL_SAFE_NO_PAD
            .decode(cursor_text)
            .map(Self)
            .map_err(|_| StreamCursorError::InvalidBase64)
    }
}

fn compute_auth_tag(
    cursor_auth_key: [u8; 32],
    cursor_body: &[u8],
) -> Result<[u8; AUTH_TAG_LEN], StreamCursorError> {
    let mut mac = cursor_mac(cursor_auth_key)?;
    mac.update(cursor_body);

    let digest = mac.finalize().into_bytes();
    let mut auth_tag = [0; AUTH_TAG_LEN];
    auth_tag.copy_from_slice(&digest);
    Ok(auth_tag)
}

fn verify_auth_tag(
    cursor_auth_key: [u8; 32],
    cursor_body: &[u8],
    auth_tag: &[u8],
) -> Result<(), StreamCursorError> {
    let mut mac = cursor_mac(cursor_auth_key)?;
    mac.update(cursor_body);
    mac.verify_slice(auth_tag)
        .map_err(|_| StreamCursorError::InvalidAuthTag)
}

fn cursor_mac(cursor_auth_key: [u8; 32]) -> Result<HmacSha256, StreamCursorError> {
    <HmacSha256 as Mac>::new_from_slice(&cursor_auth_key)
        .map_err(|_| StreamCursorError::InvalidAuthKey)
}

fn read_u32_be(bytes: &[u8], offset: usize) -> Result<u32, StreamCursorError> {
    let end = offset
        .checked_add(4)
        .ok_or(StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    let number_bytes = bytes
        .get(offset..end)
        .ok_or(StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    let number_bytes =
        <[u8; 4]>::try_from(number_bytes).map_err(|_| StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    Ok(u32::from_be_bytes(number_bytes))
}

fn read_u64_be(bytes: &[u8], offset: usize) -> Result<u64, StreamCursorError> {
    let end = offset
        .checked_add(8)
        .ok_or(StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    let number_bytes = bytes
        .get(offset..end)
        .ok_or(StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    let number_bytes =
        <[u8; 8]>::try_from(number_bytes).map_err(|_| StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    Ok(u64::from_be_bytes(number_bytes))
}

/// Error returned while decoding a stream cursor token.
#[derive(Debug, Error)]
pub enum StreamCursorError {
    /// Cursor byte length is not the fixed v1 length.
    #[error("stream cursor has invalid length: {byte_count} bytes")]
    InvalidLength {
        /// Cursor byte length.
        byte_count: usize,
    },

    /// Cursor schema version is not supported.
    #[error("stream cursor schema version {version} is unsupported")]
    UnsupportedSchemaVersion {
        /// Unsupported schema version.
        version: u8,
    },

    /// Cursor network id is unknown.
    #[error("stream cursor network id {network_id} is unknown")]
    UnknownNetwork {
        /// Unknown network id.
        network_id: u32,
    },

    /// Cursor belongs to a different network.
    #[error("stream cursor network mismatch: expected {expected:?}, actual {actual:?}")]
    NetworkMismatch {
        /// Expected network.
        expected: Network,
        /// Actual cursor network.
        actual: Network,
    },

    /// Cursor belongs to a different stream family.
    #[error("stream cursor family mismatch: flags {flags}")]
    StreamFamilyMismatch {
        /// Cursor flags byte.
        flags: u8,
    },

    /// Cursor authentication tag does not match its body.
    #[error("stream cursor authentication tag is invalid")]
    InvalidAuthTag,

    /// Store cursor authentication key could not initialize the MAC.
    #[error("stream cursor authentication key is invalid")]
    InvalidAuthKey,

    /// Cursor text is not valid unpadded base64url.
    #[error("stream cursor is not valid base64url")]
    InvalidBase64,
}

#[cfg(test)]
mod tests {
    use zinder_core::{BlockHash, BlockHeight, Network};

    use super::{
        ChainEventCursorAnchor, ChainEventStreamFamily, STREAM_CURSOR_TOKEN_V1_LEN,
        StreamCursorError, StreamCursorTokenV1,
    };

    const CURSOR_AUTH_KEY: [u8; 32] = [7; 32];

    #[test]
    fn chain_event_cursor_round_trips_through_base64url() -> Result<(), StreamCursorError> {
        let cursor = test_cursor()?;
        let cursor_text = cursor.to_base64url();
        let decoded = StreamCursorTokenV1::from_base64url(&cursor_text)?;

        assert_eq!(decoded, cursor);
        assert!(!cursor_text.contains('='));

        Ok(())
    }

    #[test]
    fn chain_event_cursor_keeps_v1_byte_offsets() -> Result<(), StreamCursorError> {
        let cursor = test_cursor()?;
        let cursor_bytes = cursor.as_bytes();

        assert_eq!(cursor_bytes.len(), STREAM_CURSOR_TOKEN_V1_LEN);
        assert_eq!(cursor_bytes[0], 1);
        assert_eq!(
            &cursor_bytes[1..5],
            &Network::ZcashRegtest.id().to_be_bytes()
        );
        assert_eq!(&cursor_bytes[5..13], &42_u64.to_be_bytes());
        assert_eq!(&cursor_bytes[13..17], &7_u32.to_be_bytes());
        assert_eq!(&cursor_bytes[17..49], &[9; 32]);
        assert_eq!(cursor_bytes[49], ChainEventStreamFamily::Tip.flags());
        assert_eq!(cursor_bytes[50..].len(), 32);

        Ok(())
    }

    #[test]
    fn malformed_base64url_cursor_is_rejected() {
        assert!(matches!(
            StreamCursorTokenV1::from_base64url("not valid cursor text!"),
            Err(StreamCursorError::InvalidBase64)
        ));
    }

    #[test]
    fn invalid_length_cursor_is_rejected() {
        let cursor = StreamCursorTokenV1::from_bytes(vec![0]);

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::InvalidLength { byte_count: 1 })
        ));
    }

    #[test]
    fn unsupported_schema_version_is_rejected() -> Result<(), StreamCursorError> {
        let mut cursor_bytes = test_cursor()?.as_bytes().to_vec();
        cursor_bytes[0] = 2;
        let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes);

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::UnsupportedSchemaVersion { version: 2 })
        ));
        Ok(())
    }

    #[test]
    fn unknown_network_id_is_rejected() -> Result<(), StreamCursorError> {
        let mut cursor_bytes = test_cursor()?.as_bytes().to_vec();
        cursor_bytes[1..5].copy_from_slice(&9999_u32.to_be_bytes());
        let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes);

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::UnknownNetwork { network_id: 9999 })
        ));
        Ok(())
    }

    #[test]
    fn wrong_network_is_rejected() -> Result<(), StreamCursorError> {
        let cursor = test_cursor()?;

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashMainnet, CURSOR_AUTH_KEY),
            Err(StreamCursorError::NetworkMismatch {
                expected: Network::ZcashMainnet,
                actual: Network::ZcashRegtest
            })
        ));
        Ok(())
    }

    #[test]
    fn wrong_stream_family_is_rejected() -> Result<(), StreamCursorError> {
        let mut cursor_bytes = test_cursor()?.as_bytes().to_vec();
        cursor_bytes[49] = 2;
        let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes);

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::StreamFamilyMismatch { flags: 2 })
        ));
        Ok(())
    }

    #[test]
    fn invalid_auth_tag_is_rejected() -> Result<(), StreamCursorError> {
        let mut cursor_bytes = test_cursor()?.as_bytes().to_vec();
        assert!(!cursor_bytes.is_empty());
        let last_index = cursor_bytes.len() - 1;
        let last_byte = &mut cursor_bytes[last_index];
        *last_byte ^= 1;
        let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes);

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::InvalidAuthTag)
        ));
        Ok(())
    }

    fn test_cursor() -> Result<StreamCursorTokenV1, StreamCursorError> {
        StreamCursorTokenV1::chain_event(
            Network::ZcashRegtest,
            ChainEventStreamFamily::Tip,
            42,
            ChainEventCursorAnchor {
                height: BlockHeight::new(7),
                hash: BlockHash::from_bytes([9; 32]),
            },
            CURSOR_AUTH_KEY,
        )
    }
}
