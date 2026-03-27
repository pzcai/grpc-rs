/// gRPC status codes as defined in
/// https://github.com/grpc/grpc/blob/master/doc/statuscodes.md
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Code {
    Ok = 0,
    Cancelled = 1,
    Unknown = 2,
    InvalidArgument = 3,
    DeadlineExceeded = 4,
    NotFound = 5,
    AlreadyExists = 6,
    PermissionDenied = 7,
    ResourceExhausted = 8,
    FailedPrecondition = 9,
    Aborted = 10,
    OutOfRange = 11,
    Unimplemented = 12,
    Internal = 13,
    Unavailable = 14,
    DataLoss = 15,
    Unauthenticated = 16,
}

/// A gRPC status — a status code paired with an error message.
///
/// Used throughout the library to propagate RPC errors in a structured way,
/// matching the semantics of grpc-go's `status.Status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub code: Code,
    pub message: String,
}

impl Status {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Status { code, message: message.into() }
    }

    pub fn ok() -> Self {
        Status::new(Code::Ok, "")
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Status::new(Code::Internal, msg)
    }

    pub fn resource_exhausted(msg: impl Into<String>) -> Self {
        Status::new(Code::ResourceExhausted, msg)
    }

    pub fn unimplemented(msg: impl Into<String>) -> Self {
        Status::new(Code::Unimplemented, msg)
    }

    pub fn invalid_argument(msg: impl Into<String>) -> Self {
        Status::new(Code::InvalidArgument, msg)
    }

    pub fn cancelled(msg: impl Into<String>) -> Self {
        Status::new(Code::Cancelled, msg)
    }

    pub fn deadline_exceeded(msg: impl Into<String>) -> Self {
        Status::new(Code::DeadlineExceeded, msg)
    }

    pub fn unavailable(msg: impl Into<String>) -> Self {
        Status::new(Code::Unavailable, msg)
    }

    pub fn is_ok(&self) -> bool {
        self.code == Code::Ok
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rpc error: code = {:?} desc = {}", self.code, self.message)
    }
}

impl std::error::Error for Status {}
