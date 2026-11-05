//! bson_error_t: libmongoc's (domain, code, message) errors, which phongo maps to exceptions.

use std::fmt;

/// mongoc_error_domain_t
pub mod domain {
    pub const CLIENT: u32 = 1;
    pub const STREAM: u32 = 2;
    pub const PROTOCOL: u32 = 3;
    pub const CURSOR: u32 = 4;
    pub const QUERY: u32 = 5;
    pub const INSERT: u32 = 6;
    pub const SASL: u32 = 7;
    pub const BSON: u32 = 8;
    pub const MATCHER: u32 = 9;
    pub const NAMESPACE: u32 = 10;
    pub const COMMAND: u32 = 11;
    pub const COLLECTION: u32 = 12;
    pub const GRIDFS: u32 = 13;
    pub const SCRAM: u32 = 14;
    pub const SERVER_SELECTION: u32 = 15;
    pub const WRITE_CONCERN: u32 = 16;
    pub const SERVER: u32 = 17;
    pub const TRANSACTION: u32 = 18;
    pub const CLIENT_SIDE_ENCRYPTION: u32 = 19;
    pub const POOL: u32 = 20;
    pub const AZURE: u32 = 21;
    pub const GCP: u32 = 22;
}

/// mongoc_error_code_t (the ones the port uses)
pub mod code {
    pub const STREAM_INVALID_TYPE: u32 = 1;
    pub const STREAM_INVALID_STATE: u32 = 2;
    pub const STREAM_NAME_RESOLUTION: u32 = 3;
    pub const STREAM_SOCKET: u32 = 4;
    pub const STREAM_CONNECT: u32 = 5;
    pub const STREAM_NOT_ESTABLISHED: u32 = 6;
    pub const CLIENT_NOT_READY: u32 = 7;
    pub const CLIENT_TOO_BIG: u32 = 8;
    pub const CLIENT_TOO_SMALL: u32 = 9;
    pub const CLIENT_GETNONCE: u32 = 10;
    pub const CLIENT_AUTHENTICATE: u32 = 11;
    pub const CLIENT_NO_ACCEPTABLE_PEER: u32 = 12;
    pub const CLIENT_IN_EXHAUST: u32 = 13;
    pub const PROTOCOL_INVALID_REPLY: u32 = 14;
    pub const PROTOCOL_BAD_WIRE_VERSION: u32 = 15;
    pub const CURSOR_INVALID_CURSOR: u32 = 16;
    pub const QUERY_FAILURE: u32 = 17;
    pub const BSON_INVALID: u32 = 18;
    pub const MATCHER_INVALID: u32 = 19;
    pub const NAMESPACE_INVALID: u32 = 20;
    pub const NAMESPACE_INVALID_FILTER_TYPE: u32 = 21;
    pub const COMMAND_INVALID_ARG: u32 = 22;
    pub const COLLECTION_INSERT_FAILED: u32 = 23;
    pub const COLLECTION_UPDATE_FAILED: u32 = 24;
    pub const COLLECTION_DELETE_FAILED: u32 = 25;
    pub const QUERY_COMMAND_NOT_FOUND: u32 = 59;
    pub const QUERY_NOT_TAILABLE: u32 = 13051;
    pub const SERVER_SELECTION_BAD_WIRE_VERSION: u32 = 13052;
    pub const SERVER_SELECTION_FAILURE: u32 = 13053;
    pub const SERVER_SELECTION_INVALID_ID: u32 = 13054;
    pub const PROTOCOL_ERROR: u32 = 17;
    pub const WRITE_CONCERN_ERROR: u32 = 64;
    pub const DUPLICATE_KEY: u32 = 11000;
    pub const MAX_TIME_MS_EXPIRED: u32 = 50;
    pub const CHANGE_STREAM_NO_RESUME_TOKEN: u32 = 13057;
    pub const CLIENT_SESSION_FAILURE: u32 = 13058;
    pub const TRANSACTION_INVALID_STATE: u32 = 13059;
    pub const CLIENT_INVALID_ENCRYPTION_STATE: u32 = 13063;
    pub const CLIENT_INVALID_ENCRYPTION_ARG: u32 = 13064;
    pub const CLIENT_API_ALREADY_SET: u32 = 13065;
    pub const CLIENT_API_FROM_POOL: u32 = 13066;
    pub const CLIENT_INVALID_LOAD_BALANCER: u32 = 13069;
}

/// Server error codes the SDAM rules look at (mongoc-error-private.h).
pub mod server_code {
    pub const NOT_PRIMARY: u32 = 10107;
    pub const NOT_PRIMARY_NO_SECONDARY_OK: u32 = 13435;
    pub const LEGACY_NOT_PRIMARY: u32 = 10058;
    pub const INTERRUPTED_AT_SHUTDOWN: u32 = 11600;
    pub const INTERRUPTED_DUE_TO_REPL_STATE_CHANGE: u32 = 11602;
    pub const NOT_PRIMARY_OR_SECONDARY: u32 = 13436;
    pub const PRIMARY_STEPPED_DOWN: u32 = 189;
    pub const SHUTDOWN_IN_PROGRESS: u32 = 91;
}

/// MONGOC_ERROR_CATEGORY (driver) / MONGOC_ERROR_CATEGORY_SERVER
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Category {
    #[default]
    Driver,
    Server,
}

/// bson_error_t
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Error {
    pub domain: u32,
    pub code: u32,
    pub category: Category,
    pub message: String,
}

impl Error {
    pub fn new(domain: u32, code: u32, message: impl Into<String>) -> Self {
        Error { domain, code, category: Category::Driver, message: message.into() }
    }
    pub fn server(domain: u32, code: u32, message: impl Into<String>) -> Self {
        Error { domain, code, category: Category::Server, message: message.into() }
    }
    /// _mongoc_error_append: `s` appended within bson_error_t's 504-byte message buffer.
    pub fn append(&mut self, s: &str) {
        const MAX: usize = 503;
        for c in s.chars() {
            if self.message.len() + c.len_utf8() > MAX {
                break;
            }
            self.message.push(c);
        }
    }
    /// _mongoc_error_is_server
    pub fn is_server(&self) -> bool {
        self.domain == domain::SERVER || self.domain == domain::WRITE_CONCERN
    }
    /// _mongoc_error_is_shutdown
    pub fn is_shutdown(&self) -> bool {
        self.is_server() && matches!(self.code, server_code::INTERRUPTED_AT_SHUTDOWN | server_code::SHUTDOWN_IN_PROGRESS)
    }
    /// _mongoc_error_is_recovering
    pub fn is_recovering(&self) -> bool {
        if !self.is_server() {
            return false;
        }
        use server_code::*;
        match self.code {
            INTERRUPTED_AT_SHUTDOWN | INTERRUPTED_DUE_TO_REPL_STATE_CHANGE | NOT_PRIMARY_OR_SECONDARY | PRIMARY_STEPPED_DOWN | SHUTDOWN_IN_PROGRESS => true,
            code::QUERY_FAILURE => self.message.contains("not master or secondary") || self.message.contains("node is recovering"),
            _ => false,
        }
    }
    /// _mongoc_error_is_not_primary
    pub fn is_not_primary(&self) -> bool {
        if !self.is_server() || self.is_recovering() {
            return false;
        }
        use server_code::*;
        match self.code {
            NOT_PRIMARY | NOT_PRIMARY_NO_SECONDARY_OK | LEGACY_NOT_PRIMARY => true,
            code::QUERY_FAILURE => self.message.contains("not master"),
            _ => false,
        }
    }
    /// _mongoc_error_is_state_change
    pub fn is_state_change(&self) -> bool {
        self.is_recovering() || self.is_not_primary()
    }
    /// _mongoc_error_is_network
    pub fn is_network(&self) -> bool {
        self.domain == domain::STREAM || (self.domain == domain::PROTOCOL && self.code == code::PROTOCOL_INVALID_REPLY)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}
