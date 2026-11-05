//! Errors, mapped to ext-mongodb's exception classes when thrown into PHP.

use ext_php_rs::{exception::PhpException, zend::ce, zend::ClassEntry};

#[derive(Debug)]
pub enum Error {
    /// Network failure (connect, send, receive, EOF): retryable.
    Net(String),
    /// Socket or connect timeout: retryable.
    Timeout(String),
    /// No suitable server.
    Selection(String),
    /// The server answered ok: 0.
    Server { code: i32, msg: String },
    /// Malformed BSON or wire message.
    Bson(String),
    /// Bad argument / URI / unsupported option.
    Invalid(String),
    /// The caller's fiber was interrupted: a PHP exception (or exit) is pending.
    Interrupted,
}

pub type R<T> = Result<T, Error>;

impl Error {
    pub fn bson(m: &str) -> Self {
        Error::Bson(m.to_owned())
    }

    pub fn net(what: &str, addr: &str, e: std::io::Error) -> Self {
        if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) {
            Error::Timeout(format!("{what} {addr}: timed out"))
        } else {
            Error::Net(format!("{what} {addr}: {e}"))
        }
    }

    /// Network errors and the server codes of the retryable-reads spec.
    pub fn retryable(&self) -> bool {
        match self {
            Error::Net(_) | Error::Timeout(_) => true,
            Error::Server { code, .. } => RETRYABLE.contains(code),
            _ => false,
        }
    }

    /// "not primary" / "node is recovering": the server's role changed, rescan.
    pub fn state_change(&self) -> bool {
        matches!(self, Error::Server { code, .. } if STATE_CHANGE.contains(code))
    }
}

const RETRYABLE: &[i32] = &[11600, 11602, 10107, 13435, 13436, 189, 91, 7, 6, 89, 9001, 262, 134];
const STATE_CHANGE: &[i32] = &[11600, 11602, 10107, 13435, 13436, 189, 91];

fn class(name: &str) -> &'static ClassEntry {
    ClassEntry::try_find(name).unwrap_or_else(ce::exception)
}

impl From<Error> for PhpException {
    fn from(e: Error) -> Self {
        match e {
            Error::Net(m) => PhpException::new(m, 0, class("MongoDB\\Driver\\Exception\\ConnectionException")),
            Error::Timeout(m) => PhpException::new(m, 0, class("MongoDB\\Driver\\Exception\\ConnectionTimeoutException")),
            // libmongoc's MONGOC_ERROR_SERVER_SELECTION_FAILURE
            Error::Selection(m) => PhpException::new(m, 13053, class("MongoDB\\Driver\\Exception\\ConnectionTimeoutException")),
            Error::Server { code, msg } => PhpException::new(msg, code, class("MongoDB\\Driver\\Exception\\CommandException")),
            Error::Bson(m) => PhpException::new(m, 0, class("MongoDB\\Driver\\Exception\\UnexpectedValueException")),
            Error::Invalid(m) => PhpException::new(m, 0, class("MongoDB\\Driver\\Exception\\InvalidArgumentException")),
            Error::Interrupted => PhpException::from_message("mongo-php-async-driver: interrupted".into()),
        }
    }
}
