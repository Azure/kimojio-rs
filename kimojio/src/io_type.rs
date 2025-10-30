// Copyright (c) Microsoft Corporation. All rights reserved.
use std::fmt::Display;

#[repr(u8)]
#[derive(Copy, Clone, Debug, zerocopy::IntoBytes)]
pub enum IOType {
    Read,
    Write,
    Recv,
    Send,
    Accept,
    Connect,
    Close,
    FAdvise,
    FAllocate,
    Fsync,
    Link,
    MAdvise,
    Mkdir,
    Open,
    Rename,
    Shutdown,
    Stat,
    Symlink,
    SyncFileRange,
    Unlink,
    UringCmd,
    Socket,
    Timeout,
    Nop,
    NvmeCmd,
    Unknown,
}

impl Display for IOType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IOType::Read => f.write_str("read"),
            IOType::Write => f.write_str("write"),
            IOType::Recv => f.write_str("recv"),
            IOType::Send => f.write_str("send"),
            IOType::Accept => f.write_str("accept"),
            IOType::Connect => f.write_str("connect"),
            IOType::Unknown => f.write_str("unknown"),
            IOType::Close => f.write_str("close"),
            IOType::FAdvise => f.write_str("fadvise"),
            IOType::FAllocate => f.write_str("fallocate"),
            IOType::Fsync => f.write_str("fsync"),
            IOType::Link => f.write_str("link"),
            IOType::MAdvise => f.write_str("madvise"),
            IOType::Mkdir => f.write_str("mkdir"),
            IOType::Open => f.write_str("open"),
            IOType::Rename => f.write_str("rename"),
            IOType::Shutdown => f.write_str("shutdown"),
            IOType::Stat => f.write_str("stat"),
            IOType::Symlink => f.write_str("symlink"),
            IOType::SyncFileRange => f.write_str("sync_file_range"),
            IOType::Unlink => f.write_str("unlink"),
            IOType::UringCmd => f.write_str("uring_cmd"),
            IOType::Socket => f.write_str("socket"),
            IOType::Nop => f.write_str("nop"),
            IOType::Timeout => f.write_str("timeout"),
            IOType::NvmeCmd => f.write_str("nvme_cmd"),
        }
    }
}

#[cfg(test)]
mod test {
    #[test]
    fn test_io_type_display() {
        assert_eq!(format!("{}", super::IOType::Read), "read");
        assert_eq!(format!("{}", super::IOType::Write), "write");
        assert_eq!(format!("{}", super::IOType::Recv), "recv");
        assert_eq!(format!("{}", super::IOType::Send), "send");
        assert_eq!(format!("{}", super::IOType::Accept), "accept");
        assert_eq!(format!("{}", super::IOType::Connect), "connect");
        assert_eq!(format!("{}", super::IOType::Unknown), "unknown");
        assert_eq!(format!("{}", super::IOType::Close), "close");
        assert_eq!(format!("{}", super::IOType::FAdvise), "fadvise");
        assert_eq!(format!("{}", super::IOType::FAllocate), "fallocate");
        assert_eq!(format!("{}", super::IOType::Fsync), "fsync");
        assert_eq!(format!("{}", super::IOType::Link), "link");
        assert_eq!(format!("{}", super::IOType::MAdvise), "madvise");
        assert_eq!(format!("{}", super::IOType::Mkdir), "mkdir");
        assert_eq!(format!("{}", super::IOType::Open), "open");
        assert_eq!(format!("{}", super::IOType::Rename), "rename");
        assert_eq!(format!("{}", super::IOType::Shutdown), "shutdown");
        assert_eq!(format!("{}", super::IOType::Stat), "stat");
        assert_eq!(format!("{}", super::IOType::Symlink), "symlink");
        assert_eq!(
            format!("{}", super::IOType::SyncFileRange),
            "sync_file_range"
        );
        assert_eq!(format!("{}", super::IOType::Unlink), "unlink");
        assert_eq!(format!("{}", super::IOType::UringCmd), "uring_cmd");
        assert_eq!(format!("{}", super::IOType::Socket), "socket");
        assert_eq!(format!("{}", super::IOType::Nop), "nop");
        assert_eq!(format!("{}", super::IOType::Timeout), "timeout");
        assert_eq!(format!("{}", super::IOType::NvmeCmd), "nvme_cmd");
    }
}
