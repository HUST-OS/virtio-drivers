use super::*;
use crate::header::VirtIOHeader;
use crate::queue::VirtQueue;
use bitflags::*;
use core::hint::spin_loop;
use volatile::Volatile;

const QUEUE_SIZE : usize = 16;
/// The virtio block device is a simple virtual block device (ie. disk).
///
/// Read and write requests (and other exotic requests) are placed in the queue,
/// and serviced (probably out of order) by the device except where noted.
pub struct VirtIOBlk<'a> {
    header: &'static mut VirtIOHeader,
    queue: VirtQueue<'a>,
    capacity: usize,
    req_queue: [BlkReq; QUEUE_SIZE],
}

impl VirtIOBlk<'_> {
    /// Create a new VirtIO-Blk driver.
    pub fn new(header: &'static mut VirtIOHeader) -> Result<Self> {
        assert!(header.verify());
        header.begin_init(|features| {
            let features = BlkFeature::from_bits_truncate(features);
            println!("device features: {:?}", features);
            // negotiate these flags only
            let supported_features = BlkFeature::empty();
            (features & supported_features).bits()
        });

        // read configuration space
        let config = unsafe { &mut *(header.config_space() as *mut BlkConfig) };
        println!("config: {:?}", config);
        println!(
            "found a block device of size {} KB",
            config.capacity.read() / 2
        );

        let queue = VirtQueue::new(header, 0, QUEUE_SIZE as u16)?;
        header.finish_init();

        Ok(VirtIOBlk {
            header,
            queue,
            capacity: config.capacity.read() as usize,
            req_queue: [BlkReq {
                type_: ReqType::In,
                reserved: 0,
                sector: 0
            }; QUEUE_SIZE]
        })
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> bool {
        self.header.ack_interrupt()
    }

    /// Read a block.
    pub fn read_block(&mut self, block_id: usize, buf: &mut [u8]) -> Result {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::In,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut resp = BlkResp::default();
        self.queue.add(&[req.as_buf()], &[buf, resp.as_buf_mut()])?;
        self.header.notify(0);
        while !self.queue.can_pop() {
            spin_loop();
        }
        self.queue.pop_used()?;
        match resp.status {
            RespStatus::Ok => Ok(()),
            _ => Err(Error::IoError),
        }
    }

    /// 非阻塞读取块
    pub fn read_block_non_block(&mut self, block_id: usize, buf: &mut [u8]) -> nb::Result<(), Error> {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::In,
            reserved: 0,
            sector: block_id as u64,
        };
        let idx = self.queue.get_desc_idx();
        self.req_queue[idx] = req;
        let mut resp = BlkResp::default();
        self.queue.add(&[self.req_queue[idx].as_buf()], &[buf, resp.as_buf_mut()])?;
        self.header.notify(0);
        match self.queue.can_pop() {
            // 读操作已经完成
            true => {
                self.queue.pop_used()?;
                match resp.status {
                    RespStatus::Ok => Ok(()),
                    _ => Err(nb::Error::Other(Error::IoError)),
                }
            },
            // 读操作没有完成，不阻塞直接返回
            // 读操作完成时通过外部中断通知操作系统内核
            false => {
                Err(nb::Error::WouldBlock)
            }
        }
    }

    /// Write a block.
    pub fn write_block(&mut self, block_id: usize, buf: &[u8]) -> Result {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::Out,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut resp = BlkResp::default();
        self.queue.add(&[req.as_buf(), buf], &[resp.as_buf_mut()])?;
        self.header.notify(0);
        while !self.queue.can_pop() {
            spin_loop();
        }
        self.queue.pop_used()?;
        match resp.status {
            RespStatus::Ok => Ok(()),
            _ => Err(Error::IoError),
        }
    }

    /// 非阻塞写入块
    pub fn write_block_non_block(&mut self, block_id: usize, buf: &[u8]) -> nb::Result<(), Error> {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::Out,
            reserved: 0,
            sector: block_id as u64,
        };
        let idx = self.queue.get_desc_idx();
        self.req_queue[idx] = req;
        let mut resp = BlkResp::default();
        self.queue.add(&[self.req_queue[idx].as_buf(), buf], &[resp.as_buf_mut()])?;
        self.header.notify(0);
        match self.queue.can_pop() {
            // 读操作已经完成
            true => {
                self.queue.pop_used()?;
                match resp.status {
                    RespStatus::Ok => Ok(()),
                    _ => Err(nb::Error::Other(Error::IoError))
                }
            },
            // 读操作没有完成，不阻塞直接返回
            // 读操作完成时通过 virtio 外部中断通知操作系统内核
            false => {
                Err(nb::Error::WouldBlock)
            }
        }
    }

    /// 处理 virtio 外部中断
    pub fn handle_interrupt(&mut self) -> Result<ReadyBlock> {
        if !self.queue.can_pop() { return Err(Error::IoError); }
        let (idx, _len) = self.queue.pop_used()
            .expect("[virtio_blk] virtio block device data not ready but enter its interrupt handler.");
        let desc = self.queue.get_desc(idx as usize);
        let req = unsafe { &*(desc.addr.read() as *const BlkReq) };
        let ret = match req.type_ {
            ReqType::In => ReadyBlock::Read(req.sector as usize),
            ReqType::Out => ReadyBlock::Write(req.sector as usize),
            _ => ReadyBlock::Other
        };
        Ok(ret)
    }
}

#[repr(C)]
#[derive(Debug)]
struct BlkConfig {
    /// Number of 512 Bytes sectors
    capacity: Volatile<u64>,
    size_max: Volatile<u32>,
    seg_max: Volatile<u32>,
    cylinders: Volatile<u16>,
    heads: Volatile<u8>,
    sectors: Volatile<u8>,
    blk_size: Volatile<u32>,
    physical_block_exp: Volatile<u8>,
    alignment_offset: Volatile<u8>,
    min_io_size: Volatile<u16>,
    opt_io_size: Volatile<u32>,
    // ... ignored
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct BlkReq {
    type_: ReqType,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
#[derive(Debug)]
struct BlkResp {
    status: RespStatus,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy)]
enum ReqType {
    In = 0,
    Out = 1,
    Flush = 4,
    Discard = 11,
    WriteZeroes = 13,
}

#[repr(u8)]
#[derive(Debug, Eq, PartialEq)]
enum RespStatus {
    Ok = 0,
    IoErr = 1,
    Unsupported = 2,
    _NotReady = 3,
}

impl Default for BlkResp {
    fn default() -> Self {
        BlkResp {
            status: RespStatus::_NotReady,
        }
    }
}

const BLK_SIZE: usize = 512;

bitflags! {
    struct BlkFeature: u64 {
        /// Device supports request barriers. (legacy)
        const BARRIER       = 1 << 0;
        /// Maximum size of any single segment is in `size_max`.
        const SIZE_MAX      = 1 << 1;
        /// Maximum number of segments in a request is in `seg_max`.
        const SEG_MAX       = 1 << 2;
        /// Disk-style geometry specified in geometry.
        const GEOMETRY      = 1 << 4;
        /// Device is read-only.
        const RO            = 1 << 5;
        /// Block size of disk is in `blk_size`.
        const BLK_SIZE      = 1 << 6;
        /// Device supports scsi packet commands. (legacy)
        const SCSI          = 1 << 7;
        /// Cache flush command support.
        const FLUSH         = 1 << 9;
        /// Device exports information on optimal I/O alignment.
        const TOPOLOGY      = 1 << 10;
        /// Device can toggle its cache between writeback and writethrough modes.
        const CONFIG_WCE    = 1 << 11;
        /// Device can support discard command, maximum discard sectors size in
        /// `max_discard_sectors` and maximum discard segment number in
        /// `max_discard_seg`.
        const DISCARD       = 1 << 13;
        /// Device can support write zeroes command, maximum write zeroes sectors
        /// size in `max_write_zeroes_sectors` and maximum write zeroes segment
        /// number in `max_write_zeroes_seg`.
        const WRITE_ZEROES  = 1 << 14;

        // device independent
        const NOTIFY_ON_EMPTY       = 1 << 24; // legacy
        const ANY_LAYOUT            = 1 << 27; // legacy
        const RING_INDIRECT_DESC    = 1 << 28;
        const RING_EVENT_IDX        = 1 << 29;
        const UNUSED                = 1 << 30; // legacy
        const VERSION_1             = 1 << 32; // detect legacy

        // the following since virtio v1.1
        const ACCESS_PLATFORM       = 1 << 33;
        const RING_PACKED           = 1 << 34;
        const IN_ORDER              = 1 << 35;
        const ORDER_PLATFORM        = 1 << 36;
        const SR_IOV                = 1 << 37;
        const NOTIFICATION_DATA     = 1 << 38;
    }
}

unsafe impl AsBuf for BlkReq {}
unsafe impl AsBuf for BlkResp {}

/// 中断响应返回准备好的块
pub enum ReadyBlock {
    /// 读请求完成的块
    Read(usize),
    /// 写操作请求完成的块
    Write(usize),
    /// 其他
    Other
}