use core::mem::size_of;
use core::slice;
use core::sync::atomic::{fence, Ordering};

use super::*;
use crate::header::VirtIOHeader;
use bitflags::*;

use volatile::Volatile;

/// The mechanism for bulk data transport on virtio devices.
///
/// Each device can have zero or more virtqueues.
#[repr(C)]
pub struct VirtQueue<'a> {
    /// DMA guard
    dma: DMA,
    /// 描述符表
    desc: &'a mut [Descriptor],
    /// 可用环
    avail: &'a mut AvailRing,
    /// 已用环
    used: &'a mut UsedRing,
    /// 虚拟队列索引值
    /// 一个虚拟设备实现可能跟有多个虚拟队列
    queue_idx: u32,
    /// 虚拟队列长度
    queue_size: u16,
    /// 已经使用的队列项目数
    num_used: u16,
    /// 空闲描述符链表头
    /// 初始时所有描述符通过 next 指针依次相连形成空闲链表
    free_head: u16,
    /// 可用环的索引值
    avail_idx: u16,
    /// 设备上次已取的已用环元素位置
    last_used_idx: u16,
}

impl VirtQueue<'_> {
    /// Create a new VirtQueue.
    pub fn new(header: &mut VirtIOHeader, idx: usize, size: u16) -> Result<Self> {
        if header.queue_used(idx as u32) {
            return Err(Error::AlreadyUsed);
        }
        if !size.is_power_of_two() || header.max_queue_size() < size as u32 {
            return Err(Error::InvalidParam);
        }
        let layout = VirtQueueLayout::new(size);
        // alloc continuous pages
        let dma = DMA::new(layout.size / PAGE_SIZE)?;
        // println!("dma.paddr: {:#x}", dma.paddr());

        header.queue_set(idx as u32, size as u32, PAGE_SIZE as u32, dma.pfn());

        // 描述符表起始地址
        let desc =
            unsafe { slice::from_raw_parts_mut(dma.vaddr() as *mut Descriptor, size as usize) };
        // 可用环起始地址
        let avail = unsafe { &mut *((dma.vaddr() + layout.avail_offset) as *mut AvailRing) };
        // 已用环起始地址
        let used = unsafe { &mut *((dma.vaddr() + layout.used_offset) as *mut UsedRing) };

        // 将空闲描述符连成链表
        for i in 0..(size - 1) {
            desc[i as usize].next.write(i + 1);
        }

        Ok(VirtQueue {
            dma,
            desc,
            avail,
            used,
            queue_size: size,
            queue_idx: idx as u32,
            num_used: 0,
            free_head: 0,
            avail_idx: 0,
            last_used_idx: 0,
        })
    }

    /// Add buffers to the virtqueue, return a token.
    ///
    /// Ref: linux virtio_ring.c virtqueue_add
    pub fn add(&mut self, inputs: &[&[u8]], outputs: &[&mut [u8]]) -> Result<u16> {
        if inputs.is_empty() && outputs.is_empty() {
            return Err(Error::InvalidParam);
        }
        if inputs.len() + outputs.len() + self.num_used as usize > self.queue_size as usize {
            // todo: 这里错误语义不符
            return Err(Error::BufferTooSmall);
        }

        // 从空闲描述符链表中分配描述符
        let head = self.free_head;
        let mut last = self.free_head;
        for input in inputs.iter() {
            let desc = &mut self.desc[self.free_head as usize];
            desc.set_buf(input);
            desc.flags.write(DescFlags::NEXT);
            last = self.free_head;
            self.free_head = desc.next.read();
        }
        for output in outputs.iter() {
            let desc = &mut self.desc[self.free_head as usize];
            desc.set_buf(output);
            desc.flags.write(DescFlags::NEXT | DescFlags::WRITE);
            last = self.free_head;
            self.free_head = desc.next.read();
        }
        // 设置描述符链的最后一个元素的 next 位为 0
        {
            let desc = &mut self.desc[last as usize];
            let mut flags = desc.flags.read();
            flags.remove(DescFlags::NEXT);
            desc.flags.write(flags);
        }
        self.num_used += (inputs.len() + outputs.len()) as u16;

        // 将描述符链的头部吸入可用环中
        let avail_slot = self.avail_idx & (self.queue_size - 1);
        self.avail.ring[avail_slot as usize].write(head);

        // write barrier(内存屏障操作？)
        fence(Ordering::SeqCst);

        // increase head of avail ring
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.avail.idx.write(self.avail_idx);
        Ok(head)
    }

    /// Whether there is a used element that can pop.
    pub fn can_pop(&self) -> bool {
        self.last_used_idx != self.used.idx.read()
    }

    /// The number of free descriptors.
    pub fn available_desc(&self) -> usize {
        (self.queue_size - self.num_used) as usize
    }

    /// Recycle descriptors in the list specified by head.
    ///
    /// This will push all linked descriptors at the front of the free list.
    fn recycle_descriptors(&mut self, mut head: u16) {
        let origin_free_head = self.free_head;
        self.free_head = head;
        loop {
            let desc = &mut self.desc[head as usize];
            let flags = desc.flags.read();
            self.num_used -= 1;
            if flags.contains(DescFlags::NEXT) {
                head = desc.next.read();
            } else {
                desc.next.write(origin_free_head);
                return;
            }
        }
    }

    /// Get a token from device used buffers, return (token, len).
    ///
    /// Ref: linux virtio_ring.c virtqueue_get_buf_ctx
    pub fn pop_used(&mut self) -> Result<(u16, u32)> {
        if !self.can_pop() {
            return Err(Error::NotReady);
        }
        // read barrier
        fence(Ordering::SeqCst);

        let last_used_slot = self.last_used_idx & (self.queue_size - 1);
        let index = self.used.ring[last_used_slot as usize].id.read() as u16;
        let len = self.used.ring[last_used_slot as usize].len.read();

        self.recycle_descriptors(index);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        Ok((index, len))
    }

    pub fn get_desc(&self, id : usize) -> Descriptor {
        self.desc[id].clone()
    }
}

/// The inner layout of a VirtQueue.
///
/// Ref: 2.6.2 Legacy Interfaces: A Note on Virtqueue Layout
struct VirtQueueLayout {
    avail_offset: usize,
    used_offset: usize,
    size: usize,
}

impl VirtQueueLayout {
    fn new(queue_size: u16) -> Self {
        assert!(
            queue_size.is_power_of_two(),
            "queue size should be a power of 2"
        );
        let queue_size = queue_size as usize;
        let desc = size_of::<Descriptor>() * queue_size;
        let avail = size_of::<u16>() * (3 + queue_size);
        let used = size_of::<u16>() * 3 + size_of::<UsedElem>() * queue_size;
        VirtQueueLayout {
            avail_offset: desc,
            used_offset: align_up(desc + avail),
            size: align_up(desc + avail) + align_up(used),
        }
    }
}

#[repr(C, align(16))]
#[derive(Debug)]
pub struct Descriptor {
    pub addr: Volatile<u64>,
    len: Volatile<u32>,
    flags: Volatile<DescFlags>,
    next: Volatile<u16>,
}

impl Clone for Descriptor {
    fn clone(&self) -> Self {
        Self {
            addr : Volatile::<u64>::new(self.addr.read()),
            len : Volatile::<u32>::new(self.len.read()),
            flags : Volatile::<DescFlags>::new(self.flags.read()),
            next : Volatile::<u16>::new(self.next.read()),
        }
    }
}

impl Descriptor {
    fn set_buf(&mut self, buf: &[u8]) {
        self.addr.write(virt_to_phys(buf.as_ptr() as usize) as u64);
        self.len.write(buf.len() as u32);
    }
}

bitflags! {
    /// Descriptor flags
    struct DescFlags: u16 {
        const NEXT = 1;
        const WRITE = 2;
        const INDIRECT = 4;
    }
}

/// The driver uses the available ring to offer buffers to the device:
/// each ring entry refers to the head of a descriptor chain.
/// It is only written by the driver and read by the device.
#[repr(C)]
#[derive(Debug)]
struct AvailRing {
    /// 与通知机制相关
    flags: Volatile<u16>,
    ///  最新放入 IO 请求的编号
    /// 从零开始递增
    idx: Volatile<u16>,
    ring: [Volatile<u16>; 32], // actual size: queue_size
    used_event: Volatile<u16>, // unused
}

/// The used ring is where the device returns buffers once it is done with them:
/// it is only written to by the device, and read by the driver.
#[repr(C)]
#[derive(Debug)]
struct UsedRing {
    flags: Volatile<u16>,
    idx: Volatile<u16>,
    ring: [UsedElem; 32],       // actual size: queue_size
    avail_event: Volatile<u16>, // unused
}

#[repr(C)]
#[derive(Debug)]
struct UsedElem {
    id: Volatile<u32>,
    len: Volatile<u32>,
}
