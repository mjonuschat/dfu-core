use super::*;
use std::convert::TryFrom;
use std::io::Cursor;
use std::prelude::v1::*;

const REQUEST_TYPE: u8 = 0b00100001;
const DFU_DNLOAD: u8 = 1;
const DFU_UPLOAD: u8 = 2;
const DFU_GETSTATUS: u8 = 3;
const DFU_ABORT: u8 = 6;
const STM32_DFU_CMD_SET_ADDRESS: u8 = 0x21;
const STM32_DFU_FIRST_DATA_BLOCK: u16 = 2;

struct Buffer<R: std::io::Read> {
    reader: R,
    buf: Box<[u8]>,
    level: usize,
}

impl<R: std::io::Read> Buffer<R> {
    fn new(size: usize, reader: R) -> Self {
        Self {
            reader,
            buf: vec![0; size].into_boxed_slice(),
            level: 0,
        }
    }

    fn fill_buf(&mut self) -> Result<&[u8], std::io::Error> {
        while self.level < self.buf.len() {
            let dst = &mut self.buf[self.level..];
            let r = self.reader.read(dst)?;
            if r == 0 {
                break;
            } else {
                self.level += r;
            }
        }
        Ok(&self.buf[0..self.level])
    }

    fn consume(&mut self, amt: usize) {
        if amt >= self.level {
            self.level = 0;
        } else {
            self.buf.copy_within(amt..self.level, 0);
            self.level -= amt;
        }
    }
}

/// Generic synchronous implementation of DFU.
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
pub struct DfuSync<IO, E>
where
    IO: DfuIo<Read = usize, Write = usize, Reset = (), Error = E>,
    E: From<std::io::Error> + From<Error>,
{
    io: IO,
    dfu: DfuSansIo,
    buffer: Vec<u8>,
    progress: Option<Box<dyn FnMut(usize)>>,
}

impl<IO, E> DfuSync<IO, E>
where
    IO: DfuIo<Read = usize, Write = usize, Reset = (), Error = E>,
    E: From<std::io::Error> + From<Error>,
{
    /// Create a new instance of a generic synchronous implementation of DFU.
    pub fn new(io: IO) -> Self {
        let transfer_size = io.functional_descriptor().transfer_size as usize;
        let descriptor = *io.functional_descriptor();

        Self {
            io,
            dfu: DfuSansIo::new(descriptor),
            buffer: vec![0x00; transfer_size],
            progress: None,
        }
    }

    /// Override the address onto which the firmware is downloaded.
    ///
    /// This address is only used if the device uses the DfuSe protocol.
    pub fn override_address(&mut self, address: u32) -> &mut Self {
        self.dfu.set_address(address);
        self
    }

    /// Use this closure to show progress.
    pub fn with_progress(&mut self, progress: impl FnMut(usize) + 'static) -> &mut Self {
        self.progress = Some(Box::new(progress));
        self
    }

    /// Consume the object and return its [`DfuIo`]
    pub fn into_inner(self) -> IO {
        self.io
    }
}

impl<IO, E> DfuSync<IO, E>
where
    IO: DfuIo<Read = usize, Write = usize, Reset = (), Error = E>,
    E: From<std::io::Error> + From<Error>,
{
    /// Download a firmware into the device from a slice.
    ///
    /// Returns `Some(Self)` if the device stayed on the bus (manifestation tolerant, no USB reset
    /// occurred) or `None` if a USB reset was performed.
    pub fn download_from_slice(self, slice: &[u8]) -> Result<Option<Self>, IO::Error> {
        let length = slice.len();
        let cursor = Cursor::new(slice);
        self.download(
            cursor,
            u32::try_from(length).map_err(|_| Error::OutOfCapabilities)?,
        )
    }

    /// Download a firmware into the device from a reader.
    ///
    /// Returns `Some(Self)` if the device stayed on the bus (manifestation tolerant, no USB reset
    /// occurred) or `None` if a USB reset was performed.
    pub fn download<R: std::io::Read>(
        mut self,
        reader: R,
        length: u32,
    ) -> Result<Option<Self>, IO::Error> {
        let transfer_size = self.io.functional_descriptor().transfer_size as usize;
        let mut reader = Buffer::new(transfer_size, reader);
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(Some(self));
        }

        macro_rules! wait_status {
            ($cmd:expr) => {{
                let mut cmd = $cmd;
                loop {
                    cmd = match cmd.next() {
                        get_status::Step::Break(cmd) => break cmd,
                        get_status::Step::Wait(cmd, poll_timeout) => {
                            std::thread::sleep(std::time::Duration::from_millis(poll_timeout));
                            let (cmd, mut control) = cmd.get_status(&mut self.buffer);
                            let n = control.execute(&self.io)?;
                            cmd.chain(&self.buffer[..n as usize])??
                        }
                    };
                }
            }};
        }

        let cmd = self.dfu.download(self.io.protocol(), length)?;
        let (cmd, mut control) = cmd.get_status(&mut self.buffer);
        let n = control.execute(&self.io)?;
        let (cmd, control) = cmd.chain(&self.buffer[..n])?;
        if let Some(control) = control {
            control.execute(&self.io)?;
        }
        let (cmd, mut control) = cmd.get_status(&mut self.buffer);
        let n = control.execute(&self.io)?;
        let mut download_loop = cmd.chain(&self.buffer[..n])??;

        loop {
            download_loop = match download_loop.next() {
                download::Step::Break => break Ok(Some(self)),
                download::Step::Erase(cmd) => {
                    let (cmd, control) = cmd.erase()?;
                    control.execute(&self.io)?;
                    wait_status!(cmd)
                }
                download::Step::SetAddress(cmd) => {
                    let (cmd, control) = cmd.set_address();
                    control.execute(&self.io)?;
                    wait_status!(cmd)
                }
                download::Step::DownloadChunk(cmd) => {
                    let chunk = reader.fill_buf()?;
                    let (cmd, control) = cmd.download(chunk)?;
                    let n = control.execute(&self.io)?;
                    reader.consume(n);
                    if let Some(progress) = self.progress.as_mut() {
                        progress(n);
                    }
                    wait_status!(cmd)
                }
                download::Step::UsbReset => {
                    log::trace!("Device reset");
                    self.io.usb_reset()?;
                    break Ok(None);
                }
            }
        }
    }

    /// Download a firmware into the device.
    ///
    /// The length is inferred from the reader. Returns `Some(Self)` if the device stayed on the
    /// bus (manifestation tolerant, no USB reset occurred) or `None` if a USB reset was performed.
    pub fn download_all<R: std::io::Read + std::io::Seek>(
        self,
        mut reader: R,
    ) -> Result<Option<Self>, IO::Error> {
        let length = u32::try_from(reader.seek(std::io::SeekFrom::End(0))?)
            .map_err(|_| Error::MaximumTransferSizeExceeded)?;
        reader.seek(std::io::SeekFrom::Start(0))?;
        self.download(reader, length)
    }

    /// Reads bytes from a DfuSe device at `address`.
    ///
    /// The requested range is read in the device's advertised transfer size.
    /// The returned wrapper can be reused for subsequent DFU operations.
    pub fn upload_from_address(
        mut self,
        address: u32,
        length: usize,
    ) -> Result<(Self, Vec<u8>), IO::Error> {
        if !matches!(self.io.protocol(), DfuProtocol::Dfuse { .. }) {
            return Err(Error::UnknownProtocol.into());
        }

        let mut command = [0; 5];
        command[0] = STM32_DFU_CMD_SET_ADDRESS;
        command[1..].copy_from_slice(&address.to_le_bytes());
        self.io
            .write_control(REQUEST_TYPE, DFU_DNLOAD, 0, &command)?;
        self.wait_for_download_idle()?;
        self.io.write_control(REQUEST_TYPE, DFU_ABORT, 0, &[])?;

        let transfer_size = self.io.functional_descriptor().transfer_size as usize;
        let mut readback = Vec::with_capacity(length);
        let mut block = STM32_DFU_FIRST_DATA_BLOCK;
        while readback.len() < length {
            let remaining = length - readback.len();
            let mut buffer = vec![0; transfer_size];
            let received = self
                .io
                .read_control(REQUEST_TYPE, DFU_UPLOAD, block, &mut buffer)?;
            if received == 0 {
                return Err(Error::NoSpaceLeft.into());
            }
            let length = received.min(buffer.len()).min(remaining);
            readback.extend_from_slice(&buffer[..length]);
            block = block.checked_add(1).ok_or(Error::OutOfCapabilities)?;
        }

        self.io.write_control(REQUEST_TYPE, DFU_ABORT, 0, &[])?;
        Ok((self, readback))
    }

    fn wait_for_download_idle(&mut self) -> Result<(), IO::Error> {
        loop {
            let received =
                self.io
                    .read_control(REQUEST_TYPE, DFU_GETSTATUS, 0, &mut self.buffer)?;
            if received < 6 {
                return Err(Error::ResponseTooShort {
                    got: received,
                    expected: 6,
                }
                .into());
            }
            let status = Status::from(self.buffer[0]);
            if status != Status::Ok {
                return Err(Error::StatusError(status).into());
            }
            let state = State::from(self.buffer[4]);
            match state {
                State::DfuDnloadIdle => return Ok(()),
                State::DfuDnloadSync | State::DfuDnbusy => {
                    let timeout = u64::from(self.buffer[1])
                        | (u64::from(self.buffer[2]) << 8)
                        | (u64::from(self.buffer[3]) << 16);
                    std::thread::sleep(std::time::Duration::from_millis(timeout));
                }
                state => {
                    return Err(Error::InvalidState {
                        got: state,
                        expected: State::DfuDnbusy,
                    }
                    .into());
                }
            }
        }
    }

    /// Send a Detach request to the device
    pub fn detach(&self) -> Result<(), IO::Error> {
        self.dfu.detach().execute(&self.io)?;
        Ok(())
    }

    /// Reset the USB device
    pub fn usb_reset(self) -> Result<IO::Reset, IO::Error> {
        self.io.usb_reset()
    }

    /// Returns whether the device will detach if requested
    pub fn will_detach(&self) -> bool {
        self.io.functional_descriptor().will_detach
    }

    /// Returns whether the device is manifestation tolerant
    pub fn manifestation_tolerant(&self) -> bool {
        self.io.functional_descriptor().manifestation_tolerant
    }
}
