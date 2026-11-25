//! Device, queue and tiny helpers shared by the spikes.
use std::cell::Cell;
use std::time::Instant;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::SystemInformation::*;
use windows::Win32::System::Threading::*;
use windows::core::*;

pub const GIB: u64 = 1 << 30;

pub struct Gpu {
    pub dev: ID3D12Device,
    pub queue: ID3D12CommandQueue,
    alloc: ID3D12CommandAllocator,
    fence: ID3D12Fence,
    event: HANDLE,
    val: Cell<u64>,
    pub name: String,
    pub driver: String,
}

/// Error as one whitespace-free token: `0x80070057:message`.
pub fn es(e: &Error) -> String {
    format!("0x{:08x}:{}", e.code().0 as u32, e.message().trim().replace(char::is_whitespace, "_"))
}

pub fn gbps(bytes: u64, secs: f64) -> f64 {
    bytes as f64 / secs / 1e9
}

pub fn avail_phys() -> u64 {
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe { GlobalMemoryStatusEx(&mut m).map(|_| m.ullAvailPhys).unwrap_or(4 * GIB) }
}

pub fn buf_desc(size: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

impl Gpu {
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
            let adapter = factory.EnumAdapters1(0)?;
            let d = adapter.GetDesc1()?;
            let name = String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_string();
            let driver = match adapter.CheckInterfaceSupport(&IDXGIDevice::IID) {
                Ok(v) => format!("{}.{}.{}.{}", (v >> 48) & 0xffff, (v >> 32) & 0xffff, (v >> 16) & 0xffff, v & 0xffff),
                Err(_) => "?".into(),
            };
            let mut dev: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut dev)?;
            let dev = dev.unwrap();
            let queue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default())?;
            let alloc = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let event = CreateEventW(None, false, false, None)?;
            Ok(Self { dev, queue, alloc, fence, event, val: Cell::new(0), name, driver })
        }
    }

    pub fn wait(&self) -> Result<()> {
        unsafe {
            let v = self.val.get() + 1;
            self.val.set(v);
            self.queue.Signal(&self.fence, v)?;
            if self.fence.GetCompletedValue() < v {
                self.fence.SetEventOnCompletion(v, self.event)?;
                WaitForSingleObject(self.event, INFINITE);
            }
        }
        Ok(())
    }

    /// One CopyBufferRegion on the direct queue, waited; returns seconds (submit to fence).
    pub fn copy(&self, dst: &ID3D12Resource, doff: u64, src: &ID3D12Resource, soff: u64, n: u64) -> Result<f64> {
        unsafe {
            let list: ID3D12GraphicsCommandList =
                self.dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &self.alloc, None)?;
            list.CopyBufferRegion(dst, doff, src, soff, n);
            list.Close()?;
            let t = Instant::now();
            self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
            self.wait()?;
            let s = t.elapsed().as_secs_f64();
            self.alloc.Reset()?;
            Ok(s)
        }
    }

    pub fn buffer(
        &self,
        heap: D3D12_HEAP_TYPE,
        size: u64,
        flags: D3D12_RESOURCE_FLAGS,
        state: D3D12_RESOURCE_STATES,
    ) -> Result<ID3D12Resource> {
        let hp = D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() };
        let mut r: Option<ID3D12Resource> = None;
        unsafe { self.dev.CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &buf_desc(size, flags), state, None, &mut r)? };
        Ok(r.unwrap())
    }

    pub fn default_buf(&self, size: u64) -> Result<ID3D12Resource> {
        self.buffer(D3D12_HEAP_TYPE_DEFAULT, size, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COMMON)
    }

    /// Upload buffer already filled by `f(i)` per byte.
    pub fn upload(&self, size: u64, f: impl Fn(usize) -> u8) -> Result<ID3D12Resource> {
        let r = self.buffer(D3D12_HEAP_TYPE_UPLOAD, size, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ)?;
        unsafe {
            let mut p = std::ptr::null_mut();
            r.Map(0, None, Some(&mut p))?;
            std::slice::from_raw_parts_mut(p as *mut u8, size as usize).iter_mut().enumerate().for_each(|(i, b)| *b = f(i));
        }
        Ok(r)
    }

    pub fn readback(&self, size: u64) -> Result<ID3D12Resource> {
        self.buffer(D3D12_HEAP_TYPE_READBACK, size, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST)
    }

    /// Map a readback buffer and hand out its first `n` bytes.
    pub fn read<T>(&self, r: &ID3D12Resource, n: usize, f: impl FnOnce(&[u8]) -> T) -> Result<T> {
        unsafe {
            let mut p = std::ptr::null_mut();
            r.Map(0, None, Some(&mut p))?;
            let out = f(std::slice::from_raw_parts(p as *const u8, n));
            r.Unmap(0, None);
            Ok(out)
        }
    }
}

// Not exported by the windows crate.
pub const MEM_WRITE_WATCH: windows::Win32::System::Memory::VIRTUAL_ALLOCATION_TYPE =
    windows::Win32::System::Memory::VIRTUAL_ALLOCATION_TYPE(0x0020_0000);
pub const WRITE_WATCH_FLAG_RESET: u32 = 1;
