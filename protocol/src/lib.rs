// Copyright 2021 Oxide Computer Company
use std::cmp::Ordering;
use std::net::SocketAddr;

use anyhow::bail;
use bytes::{BufMut, BytesMut};
use num_enum::IntoPrimitive;
use serde::{Deserialize, Serialize};
use strum_macros::EnumDiscriminants;
use tokio_util::codec::{Decoder, Encoder};
use uuid::Uuid;

const MAX_FRM_LEN: usize = 100 * 1024 * 1024; // 100M

use crucible_common::{Block, CrucibleError, RegionDefinition};

/// Wrapper type for a job ID
///
/// A job ID is used to identify a specific job to the downstairs.  It is used
/// in resolving dependencies.
#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    Hash,
    PartialEq,
    Ord,
    PartialOrd,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct JobId(pub u64);

impl std::fmt::Display for JobId {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> Result<(), std::fmt::Error> {
        self.0.fmt(f)
    }
}

/// Wrapper type for a reconciliation ID, used during initial startup
#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    Hash,
    PartialEq,
    Ord,
    PartialOrd,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct ReconciliationId(pub u64);

impl std::fmt::Display for ReconciliationId {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> Result<(), std::fmt::Error> {
        self.0.fmt(f)
    }
}

/// Wrapper type for a client ID
///
/// This is guaranteed by construction to be in the range `0..3`
#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    Hash,
    PartialEq,
    Ord,
    PartialOrd,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct ClientId(u8);

impl ClientId {
    /// Builds a new client ID
    ///
    /// # Panics
    /// If `i >= 3`, the ID is invalid and this constructor will panic
    pub fn new(i: u8) -> Self {
        assert!(i < 3);
        Self(i)
    }
    pub fn iter() -> impl Iterator<Item = Self> {
        (0..3).map(Self)
    }
    pub fn get(&self) -> u8 {
        self.0
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> Result<(), std::fmt::Error> {
        // TODO: this could include brackets, e.g. "[0]"
        self.0.fmt(f)
    }
}

/// Deserialized write operation
///
/// `data` should be a borrowed section of the received `Message::Write`, to
/// reduce memory copies.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct Write {
    pub eid: u64,
    pub offset: Block,
    pub data: bytes::Bytes,

    pub block_context: BlockContext,
}

/// Write data, containing data from all blocks
#[derive(Debug)]
pub struct RawWrite {
    /// Per-block metadata
    pub blocks: Vec<WriteBlockMetadata>,
    /// Raw data
    pub data: bytes::BytesMut,
}

impl RawWrite {
    /// Builds a new empty `RawWrite` with the given capacity
    pub fn with_capacity(block_count: usize, block_size: u64) -> Self {
        Self {
            blocks: Vec::with_capacity(block_count),
            data: bytes::BytesMut::with_capacity(
                block_count * block_size as usize,
            ),
        }
    }
}

#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct ReadRequest {
    pub eid: u64,
    pub offset: Block,
}

// Note: if you change this, you may have to add to the dump commands that show
// block specific data.
/// Deserialized read response
///
/// `data` should be a borrowed section of the incoming `Message::ReadResponse`,
/// to reduce memory copies.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct ReadResponse {
    pub eid: u64,
    pub offset: Block,

    pub data: bytes::BytesMut,
    pub block_contexts: Vec<BlockContext>,
}

impl ReadResponse {
    pub fn hashes(&self) -> Vec<u64> {
        self.block_contexts.iter().map(|x| x.hash).collect()
    }

    pub fn first_hash(&self) -> Option<u64> {
        self.block_contexts.first().map(|ctx| ctx.hash)
    }

    pub fn encryption_contexts(&self) -> Vec<Option<&EncryptionContext>> {
        self.block_contexts
            .iter()
            .map(|x| x.encryption_context.as_ref())
            .collect()
    }
}

/// Read response data, containing data from all blocks
#[derive(Debug)]
pub struct RawReadResponse {
    /// Per-block metadata
    pub blocks: Vec<ReadResponseBlockMetadata>,
    /// Raw data
    pub data: bytes::BytesMut,
}

impl RawReadResponse {
    /// Builds a new empty `RawReadResponse` with the given capacity
    pub fn with_capacity(block_count: usize, block_size: u64) -> Self {
        Self {
            blocks: Vec::with_capacity(block_count),
            data: bytes::BytesMut::with_capacity(
                block_count * block_size as usize,
            ),
        }
    }
}

#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlockContext {
    /// If this is a non-encrypted write, then the integrity hasher has the
    /// data as an input:
    ///
    ///   let hasher = Hasher()
    ///   hasher.write(&data)
    ///   hash = hasher.digest()
    ///
    /// If this is an encrypted write, then the integrity hasher has the
    /// nonce, then tag, then data written to it.
    ///
    ///   let hasher = Hasher()
    ///   hasher.write(&nonce)
    ///   hasher.write(&tag)
    ///   hasher.write(&data)
    ///   hash = hasher.digest()
    ///
    /// The hash is performed **after** encryption so that the downstairs can
    /// verify it without the key.
    pub hash: u64,

    pub encryption_context: Option<EncryptionContext>,
}

#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EncryptionContext {
    pub nonce: [u8; 12],
    pub tag: [u8; 16],
}

impl ReadResponse {
    pub fn from_request(request: &ReadRequest, bs: usize) -> ReadResponse {
        /*
         * XXX Some thought will need to be given to where the read
         * data buffer is created, both on this side and the remote.
         * Also, we (I) need to figure out how to read data into an
         * uninitialized buffer. Until then, we have this workaround.
         */
        let sz = bs;
        let mut data = BytesMut::with_capacity(sz);
        data.resize(sz, 1);

        ReadResponse {
            eid: request.eid,
            offset: request.offset,
            data,
            block_contexts: vec![],
        }
    }

    pub fn from_request_with_data(
        request: &ReadRequest,
        data: &[u8],
    ) -> ReadResponse {
        ReadResponse {
            eid: request.eid,
            offset: request.offset,
            data: BytesMut::from(data),
            block_contexts: vec![BlockContext {
                hash: crucible_common::integrity_hash(&[data]),
                encryption_context: None,
            }],
        }
    }
}

/**
 * These enums are for messages sent between an Upstairs and a Downstairs
 */
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct SnapshotDetails {
    pub snapshot_name: String,
}

/**
 * Convenience constants to provide some documentation on what changes have
 * been introduced in the various Crucible upstairs to downstairs versions.
 */
#[repr(u32)]
#[derive(IntoPrimitive)]
pub enum MessageVersion {
    /// Changed `Write`, `WriteUnwritten`, and `ReadResponse` variants to have a
    /// clean split between header and bulk data, to reduce `memcpy`
    ///
    /// Removed `#[repr(u16)]` and explicit variant numbering from `Message`,
    /// because those are misleading; they're ignored during serialization.
    V6 = 6,

    /// Switched to raw file extents
    ///
    /// The message format remains the same, but live repair between SQLite and
    /// raw file extents is not possible.
    V5 = 5,

    /// Added ErrorReport
    V4 = 4,

    /// Added ExtentLiveRepairAckId for LiveRepair
    V3 = 3,

    /// Initial support for LiveRepair.
    V2 = 2,

    /// Original format that remained too long.
    V1 = 1,
}
impl MessageVersion {
    pub const fn current() -> Self {
        Self::V6
    }
}

/**
 * Crucible Upstairs Downstairs message protocol version.
 * This, along with the MessageVersion enum above should be updated whenever
 * changes are made to the Message enum below.
 */
pub const CRUCIBLE_MESSAGE_VERSION: u32 = 6;

/*
 * If you add or change the Message enum, you must also increment the
 * CRUCIBLE_MESSAGE_VERSION.  It's just a few lines above you, why not
 * go do that right now before you forget.
 */
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(
    Debug, PartialEq, Clone, Serialize, Deserialize, EnumDiscriminants,
)]
#[strum_discriminants(derive(Serialize, Deserialize))]
pub enum Message {
    /**
     * Initial negotiation messages
     * This is the first message that the upstairs sends to the downstairs
     * as soon as the connection starts.
     */
    HereIAm {
        // The Message version the upstairs is running.
        version: u32,
        // The UUID of the region set.
        upstairs_id: Uuid,
        // The unique UUID just for this running upstairs session
        session_id: Uuid,
        // Generation number (IGNORED)
        gen: u64,
        // If we expect the region to be read-only.
        read_only: bool,
        // If we expect the region to be  encrypted.
        encrypted: bool,
        // Additional Message versions this upstairs supports.
        alternate_versions: Vec<u32>,
    },
    /**
     * This is the first message (when things are good) that the downstairs
     * will reply to the upstairs with.
     */
    YesItsMe {
        // The version the downstairs will be using.
        version: u32,
        // The IP:Port that repair commands will use to communicate.
        repair_addr: SocketAddr,
    },

    /*
     * These messages indicate that there is an incompatibility between the
     * upstairs and the downstairs and what the problem is.
     */
    VersionMismatch {
        // Version of Message this downstairs wanted.
        version: u32,
    },
    ReadOnlyMismatch {
        expected: bool,
    },
    EncryptedMismatch {
        expected: bool,
    },

    /**
     * Forcefully tell this downstairs to promote us (an Upstairs) to
     * active.
     *
     * Kick out the old Upstairs.
     */
    PromoteToActive {
        upstairs_id: Uuid,
        session_id: Uuid,
        gen: u64,
    },
    YouAreNowActive {
        upstairs_id: Uuid,
        session_id: Uuid,
        gen: u64,
    },
    YouAreNoLongerActive {
        new_upstairs_id: Uuid,
        new_session_id: Uuid,
        new_gen: u64,
    },

    /*
     * If downstairs sees a UUID that doesn't match what was negotiated, it
     * will send this message.
     */
    UuidMismatch {
        expected_id: Uuid,
    },

    /*
     * Ping related
     */
    Ruok,
    Imok,

    /*
     * Reconciliation related
     * These messages are used only during the initial startup process
     * when the upstairs is making all three downstairs consistent with
     * each other.
     * We use rep_id here (Repair ID) instead of job_id to be clear that
     * this is reconciliation work and not actual IO.  The reconciliation work
     * uses a different work queue and each reconciliation job must finish on
     * all three downstairs before the next one can be sent.
     */
    /// Send a close the given extent ID on the downstairs.
    ExtentClose {
        repair_id: ReconciliationId,
        extent_id: usize,
    },

    /// Send a request to reopen the given extent.
    ExtentReopen {
        repair_id: ReconciliationId,
        extent_id: usize,
    },

    /// Flush just this extent on just this downstairs client.
    ExtentFlush {
        repair_id: ReconciliationId,
        extent_id: usize,
        client_id: ClientId,
        flush_number: u64,
        gen_number: u64,
    },

    /// Replace an extent with data from the given downstairs.
    ExtentRepair {
        repair_id: ReconciliationId,
        extent_id: usize,
        source_client_id: ClientId,
        source_repair_address: SocketAddr,
        dest_clients: Vec<ClientId>,
    },

    /// The given repair job ID has finished without error
    RepairAckId {
        repair_id: ReconciliationId,
    },

    /// A problem with the given extent
    ExtentError {
        repair_id: ReconciliationId,
        extent_id: usize,
        error: CrucibleError,
    },

    /*
     * Live Repair related.
     * These messages are used to repair a downstairs while the upstairs
     * is active and receiving IOs.  These messages are sent from the
     * upstairs to the downstairs.
     */
    /// Close an extent
    ExtentLiveClose {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        dependencies: Vec<JobId>,
        extent_id: usize,
    },
    /// Flush and then close an extent.
    ExtentLiveFlushClose {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        dependencies: Vec<JobId>,
        extent_id: usize,
        flush_number: u64,
        gen_number: u64,
    },
    /// Live Repair of an extent
    ExtentLiveRepair {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        dependencies: Vec<JobId>,
        extent_id: usize,
        source_client_id: ClientId,
        source_repair_address: SocketAddr,
    },
    /// Reopen this extent, for use when upstairs is active.
    ExtentLiveReopen {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        dependencies: Vec<JobId>,
        extent_id: usize,
    },
    /// There is no real work to do, but we need to complete this job id
    ExtentLiveNoOp {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        dependencies: Vec<JobId>,
    },

    /*
     * Live Repair response messages.
     */
    /// The extent closed successfully
    /// Included are the gen and flush numbers that were committed as
    /// part of this flush request.  Note that if the extent is not
    /// dirty, then these numbers may be different than the flush/gen
    /// that was sent with the original flush
    /// This result is used for both the ExtentLiveClose and the
    /// ExtentLiveFlushClose messages.
    ExtentLiveCloseAck {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        result: Result<(u64, u64, bool), CrucibleError>,
    },

    /// The given "ExtentLiveRepair" message ID was completed.  This message
    /// will only be from ExtentLiveRepair, as this operations failure
    /// will require special action in the upstairs.
    ExtentLiveRepairAckId {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        result: Result<(), CrucibleError>,
    },

    /// The given "ExtentLive" message ID was completed.  This message
    /// will be from ExtentLiveRepair, ExtentLiveReopen, or ExtentLiveNoOp
    ExtentLiveAckId {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        result: Result<(), CrucibleError>,
    },

    /*
     * Metadata exchange
     */
    RegionInfoPlease,
    RegionInfo {
        region_def: RegionDefinition,
    },

    ExtentVersionsPlease,
    ExtentVersions {
        gen_numbers: Vec<u64>,
        flush_numbers: Vec<u64>,
        dirty_bits: Vec<bool>,
    },

    LastFlush {
        last_flush_number: JobId,
    },
    LastFlushAck {
        last_flush_number: JobId,
    },

    /*
     * IO related
     */
    Write {
        header: WriteHeader,
        data: bytes::Bytes,
    },
    WriteAck {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        result: Result<(), CrucibleError>,
    },

    Flush {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        dependencies: Vec<JobId>,
        flush_number: u64,
        gen_number: u64,
        snapshot_details: Option<SnapshotDetails>,
        /*
         * The ending extent where a flush should stop.
         * This value is unique per downstairs.
         */
        extent_limit: Option<usize>,
    },
    FlushAck {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        result: Result<(), CrucibleError>,
    },

    ReadRequest {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        dependencies: Vec<JobId>,
        requests: Vec<ReadRequest>,
    },
    ReadResponse {
        header: ReadResponseHeader,
        data: bytes::BytesMut,
    },

    WriteUnwritten {
        header: WriteHeader,
        data: bytes::Bytes,
    },
    WriteUnwrittenAck {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        result: Result<(), CrucibleError>,
    },

    ErrorReport {
        upstairs_id: Uuid,
        session_id: Uuid,
        job_id: JobId,
        error: CrucibleError,
    },

    /*
     * Misc
     */
    Unknown(u32, BytesMut),
}
/*
 * If you just added or changed the Message enum above, you must also
 * increment the CRUCIBLE_MESSAGE_VERSION.  Go do that right now before you
 * forget.
 */

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct WriteHeader {
    pub upstairs_id: Uuid,
    pub session_id: Uuid,
    pub job_id: JobId,
    pub dependencies: Vec<JobId>,
    pub blocks: Vec<WriteBlockMetadata>,
}

impl WriteHeader {
    /// Destructures into a list of block-size writes which borrow our data
    ///
    /// # Panics
    /// `buf.len()` must be an even multiple of `self.blocks.len()`, which is
    /// assumed to be the block size.
    pub fn get_writes(&self, mut buf: bytes::Bytes) -> Vec<Write> {
        assert_eq!(buf.len() % self.blocks.len(), 0);
        let block_size = buf.len() / self.blocks.len();
        let mut out = Vec::with_capacity(self.blocks.len());
        for b in &self.blocks {
            let data = buf.split_to(block_size);
            out.push(Write {
                eid: b.eid,
                offset: b.offset,
                block_context: b.block_context,
                data,
            })
        }
        out
    }
}

#[derive(Debug, PartialEq, Copy, Clone, Serialize, Deserialize)]
pub struct WriteBlockMetadata {
    pub eid: u64,
    pub offset: Block,
    pub block_context: BlockContext,
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct ReadResponseHeader {
    pub upstairs_id: Uuid,
    pub session_id: Uuid,
    pub job_id: JobId,
    pub blocks: Result<Vec<ReadResponseBlockMetadata>, CrucibleError>,
}

impl ReadResponseHeader {
    /// Destructures into a list of block-size read responses
    ///
    /// # Panics
    /// `buf.len()` must be an even multiple of `self.blocks.len()`, which is
    /// assumed to be the block size.
    pub fn into_read_responses(
        self,
        mut buf: bytes::BytesMut,
    ) -> Result<Vec<ReadResponse>, CrucibleError> {
        let blocks = self.blocks?;
        assert_eq!(buf.len() % blocks.len(), 0);
        let block_size = buf.len() / blocks.len();
        let mut out = Vec::with_capacity(blocks.len());
        for b in blocks {
            let data = buf.split_to(block_size);
            out.push(ReadResponse {
                eid: b.eid,
                offset: b.offset,
                block_contexts: b.block_contexts,
                data,
            })
        }
        Ok(out)
    }
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct ReadResponseBlockMetadata {
    pub eid: u64,
    pub offset: Block,
    pub block_contexts: Vec<BlockContext>,
}

impl ReadResponseBlockMetadata {
    pub fn hashes(&self) -> Vec<u64> {
        self.block_contexts.iter().map(|x| x.hash).collect()
    }

    pub fn first_hash(&self) -> Option<u64> {
        self.block_contexts.first().map(|ctx| ctx.hash)
    }

    pub fn encryption_contexts(&self) -> Vec<Option<&EncryptionContext>> {
        self.block_contexts
            .iter()
            .map(|x| x.encryption_context.as_ref())
            .collect()
    }
}

impl Message {
    /// Return true if this message contains an Error result
    pub fn err(&self) -> Option<&CrucibleError> {
        match self {
            Message::HereIAm { .. } => None,
            Message::YesItsMe { .. } => None,
            Message::VersionMismatch { .. } => None,
            Message::ReadOnlyMismatch { .. } => None,
            Message::EncryptedMismatch { .. } => None,
            Message::PromoteToActive { .. } => None,
            Message::YouAreNowActive { .. } => None,
            Message::YouAreNoLongerActive { .. } => None,
            Message::UuidMismatch { .. } => None,
            Message::Ruok { .. } => None,
            Message::Imok { .. } => None,
            Message::ExtentClose { .. } => None,
            Message::ExtentReopen { .. } => None,
            Message::ExtentFlush { .. } => None,
            Message::ExtentRepair { .. } => None,
            Message::RepairAckId { .. } => None,
            Message::RegionInfoPlease { .. } => None,
            Message::RegionInfo { .. } => None,
            Message::ExtentVersionsPlease { .. } => None,
            Message::ExtentVersions { .. } => None,
            Message::LastFlush { .. } => None,
            Message::LastFlushAck { .. } => None,
            Message::Write { .. } => None,
            Message::ExtentLiveClose { .. } => None,
            Message::ExtentLiveFlushClose { .. } => None,
            Message::ExtentLiveRepair { .. } => None,
            Message::ExtentLiveReopen { .. } => None,
            Message::ExtentLiveNoOp { .. } => None,
            Message::Flush { .. } => None,
            Message::ReadRequest { .. } => None,
            Message::WriteUnwritten { .. } => None,
            Message::Unknown(..) => None,

            Message::ExtentError { error, .. } => Some(error),
            Message::ErrorReport { error, .. } => Some(error),

            Message::ExtentLiveCloseAck { result, .. } => result.as_ref().err(),
            Message::ExtentLiveRepairAckId { result, .. } => {
                result.as_ref().err()
            }
            Message::ExtentLiveAckId { result, .. } => result.as_ref().err(),
            Message::WriteAck { result, .. } => result.as_ref().err(),
            Message::FlushAck { result, .. } => result.as_ref().err(),
            Message::ReadResponse { header, .. } => {
                header.blocks.as_ref().err()
            }
            Message::WriteUnwrittenAck { result, .. } => result.as_ref().err(),
        }
    }
}

// In our `Display` implementation, we skip printing large chunks of data but
// otherwise delegate to the `Debug` formatter.
impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        match self {
            Message::Write {
                header:
                    WriteHeader {
                        upstairs_id,
                        session_id,
                        job_id,
                        dependencies,
                        blocks,
                    },
                data: _,
            } => f
                .debug_struct("Message::Write")
                .field("upstairs_id", &upstairs_id)
                .field("session_id", &session_id)
                .field("job_id", &job_id)
                .field("dependencies", &dependencies)
                .field("blocks", &blocks)
                .field("writes", &"..")
                .finish(),
            Message::WriteUnwritten {
                header:
                    WriteHeader {
                        upstairs_id,
                        session_id,
                        job_id,
                        dependencies,
                        blocks,
                    },
                data: _,
            } => f
                .debug_struct("Message::WriteUnwritten")
                .field("upstairs_id", &upstairs_id)
                .field("session_id", &session_id)
                .field("job_id", &job_id)
                .field("dependencies", &dependencies)
                .field("blocks", &blocks)
                .field("writes", &"..")
                .finish(),
            Message::ReadResponse {
                header:
                    ReadResponseHeader {
                        upstairs_id,
                        session_id,
                        job_id,
                        blocks,
                    },
                data: _,
            } => f
                .debug_struct("Message::ReadResponse")
                .field("upstairs_id", &upstairs_id)
                .field("session_id", &session_id)
                .field("job_id", &job_id)
                .field("blocks", &blocks)
                .field("responses", &"..")
                .finish(),
            m => std::fmt::Debug::fmt(m, f),
        }
    }
}

/// Writer to efficiently encode and send a `Message`
///
/// In contrast with `CrucibleEncoder`, this writer will send bulk data in a
/// separate syscall (rather than copying it into an intermediate buffer).
pub struct MessageWriter<W> {
    writer: W,

    /// Scratch space for full `Message` encoding
    scratch: BytesMut,

    /// Scratch space for the raw header
    header: Vec<u8>,
}

impl<W> MessageWriter<W>
where
    W: tokio::io::AsyncWrite + std::marker::Unpin + std::marker::Send + 'static,
{
    /// Builds a new `MessageWriter`
    #[inline]
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            scratch: BytesMut::new(),
            header: vec![],
        }
    }

    /// Removes the inner type
    #[inline]
    pub fn into_inner(self) -> W {
        self.writer
    }

    async fn send_raw<H: Serialize, B: AsRef<[u8]>>(
        &mut self,
        discriminant: MessageDiscriminants,
        header: H,
        data: B,
    ) -> Result<(), CrucibleError> {
        let data = data.as_ref();
        use tokio::io::AsyncWriteExt;

        self.header.clear();
        let mut cursor = std::io::Cursor::new(&mut self.header);
        bincode::serialize_into(
            &mut cursor,
            &(
                0u32, // dummy length, to be patched later
                discriminant,
                header,
                data.len(),
            ),
        )
        .unwrap();

        // Patch the length
        let len: u32 = (self.header.len() + data.len()).try_into().unwrap();
        self.header[0..4].copy_from_slice(&len.to_le_bytes());

        // write_all_vectored would save a syscall, but is nightly-only
        self.writer.write_all(&self.header).await?;
        self.writer.write_all(data).await?;

        Ok(())
    }

    /// Sends the given message down the wire
    #[inline]
    pub async fn send(&mut self, m: Message) -> Result<(), CrucibleError> {
        use tokio::io::AsyncWriteExt;

        let discriminant = MessageDiscriminants::from(&m);
        match m {
            Message::Write { header, data } => {
                self.send_raw(discriminant, header, data).await
            }
            Message::WriteUnwritten { header, data } => {
                self.send_raw(discriminant, header, data).await
            }
            Message::ReadResponse { header, data } => {
                self.send_raw(discriminant, header, data).await
            }
            m => {
                // Serialize into our local BytesMut, to avoid allocation churn
                self.scratch.clear();
                let mut e = CrucibleEncoder::new();
                e.encode(m, &mut self.scratch)?;
                self.writer.write_all(&self.scratch).await?;
                Ok(())
            }
        }
    }
}

#[derive(Debug)]
pub struct CrucibleEncoder {}

impl CrucibleEncoder {
    pub fn new() -> Self {
        CrucibleEncoder {}
    }

    fn serialized_size<T: serde::Serialize>(
        m: T,
    ) -> Result<usize, anyhow::Error> {
        let serialized_len: usize = bincode::serialized_size(&m)? as usize;
        let len = serialized_len + 4;

        Ok(len)
    }

    fn a_write(bs: usize) -> WriteBlockMetadata {
        WriteBlockMetadata {
            eid: 1,
            offset: Block::new(1, bs.trailing_zeros()),
            block_context: BlockContext {
                hash: 0,
                encryption_context: Some(EncryptionContext {
                    nonce: [0; 12],
                    tag: [0; 16],
                }),
            },
        }
    }

    /*
     * Binary search to find the maximum number of blocks we can send.
     *
     * Attempts at deterministically computing the number of blocks
     * repeatedly failed, so binary search instead. Note that this computes
     * the maximum size that this Tokio encoding / decoding supports
     * given our constant MAX_FRM_LEN.
     */
    pub fn max_io_blocks(bs: usize) -> Result<usize, anyhow::Error> {
        let block_meta = CrucibleEncoder::a_write(bs);
        let size_of_write_message =
            CrucibleEncoder::serialized_size(CrucibleEncoder::a_write(bs))?
                + bs;

        // Maximum frame length divided by a write of one block is the lower
        // bound.
        let n = MAX_FRM_LEN / size_of_write_message;
        let lower_size_write_message = Message::Write {
            header: WriteHeader {
                upstairs_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                job_id: JobId(1),
                dependencies: vec![JobId(1)],
                blocks: vec![block_meta; n],
            },
            data: vec![0; n * bs].into(),
        };

        assert!(
            CrucibleEncoder::serialized_size(&lower_size_write_message)?
                < MAX_FRM_LEN
        );

        // The upper bound is the maximum frame length divided by the block
        // size.
        let n = MAX_FRM_LEN / bs;
        let upper_size_write_message = Message::Write {
            header: WriteHeader {
                upstairs_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                job_id: JobId(1),
                dependencies: vec![JobId(1)],
                blocks: vec![block_meta; n],
            },
            data: vec![0; n * bs].into(),
        };

        assert!(
            CrucibleEncoder::serialized_size(&upper_size_write_message)?
                > MAX_FRM_LEN
        );

        // Binary search for the number of blocks that represents the largest IO
        // given MAX_FRM_LEN.

        let mut lower = match lower_size_write_message {
            Message::Write { header, .. } => header.blocks.len(),
            _ => {
                bail!("wat");
            }
        };

        let mut upper = match upper_size_write_message {
            Message::Write { header, .. } => header.blocks.len(),
            _ => {
                bail!("wat");
            }
        };

        let mut mid = (lower + upper) / 2;

        loop {
            if (mid + 1) == upper {
                return Ok(mid);
            }

            let mid_size_write_message = Message::Write {
                header: WriteHeader {
                    upstairs_id: Uuid::new_v4(),
                    session_id: Uuid::new_v4(),
                    job_id: JobId(1),
                    dependencies: vec![JobId(1)],
                    blocks: vec![block_meta; mid],
                },
                data: vec![0; mid * bs].into(),
            };

            let mid_size =
                CrucibleEncoder::serialized_size(&mid_size_write_message)?;

            match mid_size.cmp(&MAX_FRM_LEN) {
                Ordering::Greater => {
                    upper = mid;
                }
                Ordering::Equal => {
                    return Ok(mid);
                }
                Ordering::Less => {
                    lower = mid;
                }
            }

            mid = (lower + upper) / 2;
        }
    }
}

impl Default for CrucibleEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/*
 * A frame is [len | serialized message].
 */

impl Encoder<Message> for CrucibleEncoder {
    type Error = anyhow::Error;

    fn encode(
        &mut self,
        m: Message,
        dst: &mut BytesMut,
    ) -> Result<(), Self::Error> {
        let len = CrucibleEncoder::serialized_size(&m)?;
        if len > MAX_FRM_LEN {
            // Bail out before creating a frame that the decoder will refuse to
            // deserialize
            bail!("frame is {} bytes, more than maximum {}", len, MAX_FRM_LEN);
        }

        let before = dst.len();
        dst.reserve(len);
        dst.put_u32_le(len as u32);
        bincode::serialize_into(dst.writer(), &m)?;
        debug_assert_eq!(dst.len() - before, len);

        Ok(())
    }
}

impl Encoder<&Message> for CrucibleEncoder {
    type Error = anyhow::Error;

    fn encode(
        &mut self,
        m: &Message,
        dst: &mut BytesMut,
    ) -> Result<(), Self::Error> {
        let len = CrucibleEncoder::serialized_size(m)?;
        if len > MAX_FRM_LEN {
            // Bail out before creating a frame that the decoder will refuse to
            // deserialize
            bail!("frame is {} bytes, more than maximum {}", len, MAX_FRM_LEN);
        }

        let before = dst.len();
        dst.reserve(len);
        dst.put_u32_le(len as u32);
        bincode::serialize_into(dst.writer(), &m)?;
        debug_assert_eq!(dst.len() - before, len);

        Ok(())
    }
}

pub struct CrucibleDecoder {}

impl CrucibleDecoder {
    pub fn new() -> Self {
        CrucibleDecoder {}
    }

    fn decode_raw<H: for<'a> Deserialize<'a>, F: Fn(H, BytesMut) -> Message>(
        mut bytes: BytesMut,
        f: F,
    ) -> Result<Message, bincode::Error> {
        // Deserialize header and data length, skipping the tag
        let (header, data_len): (H, usize) = bincode::deserialize(&bytes[4..])?;

        // Slice out the bulk data
        let data_start = bytes.len() - data_len;
        let data = bytes.split_off(data_start);

        // Build our message from header + bulk data
        Ok(f(header, data))
    }
}

impl Default for CrucibleDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for CrucibleDecoder {
    type Item = Message;
    type Error = anyhow::Error;

    fn decode(
        &mut self,
        src: &mut BytesMut,
    ) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 4 {
            /*
             * Wait for the u32 length prefix.
             */
            return Ok(None);
        }

        /*
         * Get the length prefix from the frame.
         */
        let mut length_bytes = [0u8; 4];
        length_bytes.copy_from_slice(&src[0..4]);
        let len = u32::from_le_bytes(length_bytes) as usize;

        if len > MAX_FRM_LEN {
            bail!("frame is {} bytes, more than maximum {}", len, MAX_FRM_LEN);
        }

        if src.len() < len {
            /*
             * Wait for an entire frame.  Expand the buffer to fit.
             */
            src.reserve(len - src.len());
            return Ok(None);
        }

        // Slice the buffer so that it contains only our bincode-serialized
        // `Message` (without any trailing data or the leading 4-byte length).
        //
        // This leaves `src` pointing to the beginning of the next packet (which
        // may not exist yet), and `buf` pointing to just our bincode-serialized
        // `Message`.
        let buf = src.split_to(len).split_off(4);

        // Deserialize just the discriminant.  This will let us decide whether
        // to use a specialized strategy for deserializing Messages that contain
        // bulk data.
        let discriminant: MessageDiscriminants =
            bincode::deserialize_from(&buf[..])?;

        let message = match discriminant {
            MessageDiscriminants::Write => {
                Self::decode_raw(buf, |header, data| Message::Write {
                    header,
                    data: data.freeze(),
                })
            }
            MessageDiscriminants::WriteUnwritten => {
                Self::decode_raw(buf, |header, data| Message::WriteUnwritten {
                    header,
                    data: data.freeze(),
                })
            }
            MessageDiscriminants::ReadResponse => {
                Self::decode_raw(buf, |header, data| Message::ReadResponse {
                    header,
                    data,
                })
            }
            _ => bincode::deserialize_from(&buf[..]),
        }?;

        Ok(Some(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    fn round_trip(input: &Message) -> Result<Message> {
        let mut enc = CrucibleEncoder::new();
        let mut buf = BytesMut::new();
        enc.encode(input.clone(), &mut buf)?;

        let mut dec = CrucibleDecoder::new();
        let output = dec.decode(&mut buf)?;
        if let Some(output) = output {
            Ok(output)
        } else {
            bail!("expected message, got None");
        }
    }

    #[test]
    fn rt_here_i_am() -> Result<()> {
        let input = Message::HereIAm {
            version: 2,
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 123,
            read_only: false,
            encrypted: true,
            alternate_versions: Vec::new(),
        };
        assert_eq!(input, round_trip(&input)?);
        Ok(())
    }

    #[test]
    fn rt_yes_its_me() -> Result<()> {
        let input = Message::YesItsMe {
            version: 20000,
            repair_addr: "127.0.0.1:123".parse().unwrap(),
        };
        assert_eq!(input, round_trip(&input)?);
        Ok(())
    }

    #[test]
    fn rt_ruok() -> Result<()> {
        let input = Message::Ruok;
        assert_eq!(input, round_trip(&input)?);
        Ok(())
    }

    #[test]
    fn rt_imok() -> Result<()> {
        let input = Message::Imok;
        assert_eq!(input, round_trip(&input)?);
        Ok(())
    }

    #[test]
    fn rt_evp() -> Result<()> {
        let input = Message::ExtentVersionsPlease;
        assert_eq!(input, round_trip(&input)?);
        Ok(())
    }

    #[test]
    fn rt_ev_0() -> Result<()> {
        let input = Message::ExtentVersions {
            gen_numbers: vec![],
            flush_numbers: vec![],
            dirty_bits: vec![],
        };
        assert_eq!(input, round_trip(&input)?);
        Ok(())
    }

    #[test]
    fn rt_ev_7() -> Result<()> {
        let input = Message::ExtentVersions {
            gen_numbers: vec![1, 2, 3, 4, u64::MAX, 1, 0],
            flush_numbers: vec![1, 2, 3, 4, u64::MAX, 1, 0],
            dirty_bits: vec![true, true, false, true, true, false, true],
        };
        assert_eq!(input, round_trip(&input)?);
        Ok(())
    }

    #[test]
    fn correctly_detect_truncated_message() -> Result<()> {
        let mut encoder = CrucibleEncoder::new();
        let mut decoder = CrucibleDecoder::new();

        let input = Message::HereIAm {
            version: 0,
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 23849183,
            read_only: true,
            encrypted: false,
            alternate_versions: Vec::new(),
        };
        let mut buffer = BytesMut::new();

        encoder.encode(input, &mut buffer)?;

        buffer.truncate(buffer.len() - 1);

        let result = decoder.decode(&mut buffer);

        match result {
            Err(_) => {
                result?;
            }
            Ok(v) => {
                assert_eq!(v, None);
            }
        };

        Ok(())
    }

    #[test]
    fn latest_message_version() {
        let cur = MessageVersion::current();
        assert_eq!(
            CRUCIBLE_MESSAGE_VERSION,
            <MessageVersion as Into<u32>>::into(cur)
        );
    }

    #[test]
    fn encoding_max_frame_length_bails() {
        let mut encoder = CrucibleEncoder::new();

        let data = bytes::Bytes::from(vec![7u8; MAX_FRM_LEN]);
        let hash = crucible_common::integrity_hash(&[&data]);

        let input = Message::Write {
            header: WriteHeader {
                upstairs_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                job_id: JobId(1),
                dependencies: vec![],
                blocks: vec![WriteBlockMetadata {
                    eid: 0,
                    offset: Block::new_512(0),
                    block_context: BlockContext {
                        hash,
                        encryption_context: None,
                    },
                }],
            },
            data,
        };

        let mut buffer = BytesMut::new();
        assert!(encoder.encode(input, &mut buffer).is_err());
    }
}
