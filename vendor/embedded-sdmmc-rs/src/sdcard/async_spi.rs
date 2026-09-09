//! # Async SD card access via SPI
//!
//! Implements the AsyncBlockDevice trait for an SD/MMC Protocol over SPI,
//! using `embedded-hal-async` traits.
//!
//! This is the async counterpart to [`super::spi`].

use super::*;
use crate::blockdevice::AsyncBlockDevice;
use crate::{Block, BlockCount, BlockIdx};

// ****************************************************************************
// Types and Implementations
// ****************************************************************************

/// Async driver for an SD Card on an SPI bus.
///
/// Built from an async [`SpiDevice`] implementation and a Delay.
///
/// Before talking to the SD Card, the caller needs to send 74 clock cycles on
/// the SPI Clock line, at 400 kHz, with no chip-select asserted (or at least,
/// not the chip-select of the SD Card).
///
/// This kind of breaks the embedded-hal model, so how to do this is left to
/// the caller. You could drive the SpiBus directly, or use an SpiDevice with
/// a dummy chip-select pin. Or you could try just not doing the 74 clocks and
/// see if your card works anyway - some do, some don't.
///
/// All the APIs take `&mut self` - no interior mutability is needed since
/// async runtimes like Embassy handle concurrency cooperatively.
///
/// [`SpiDevice`]: embedded_hal_async::spi::SpiDevice
pub struct AsyncSdCard<SPI, DELAYER>
where
    SPI: embedded_hal_async::spi::SpiDevice<u8>,
    DELAYER: embedded_hal_async::delay::DelayNs,
{
    inner: AsyncSdCardInner<SPI, DELAYER>,
}

impl<SPI, DELAYER> AsyncSdCard<SPI, DELAYER>
where
    SPI: embedded_hal_async::spi::SpiDevice<u8>,
    DELAYER: embedded_hal_async::delay::DelayNs,
{
    /// Create a new async SD/MMC Card driver using a raw SPI interface.
    ///
    /// The card will not be initialised at this time. Initialisation is
    /// deferred until a method is called on the object.
    ///
    /// Uses the default options.
    pub fn new(spi: SPI, delayer: DELAYER) -> AsyncSdCard<SPI, DELAYER> {
        Self::new_with_options(spi, delayer, super::spi::AcquireOpts::default())
    }

    /// Construct a new async SD/MMC Card driver, using a raw SPI interface and the given options.
    ///
    /// The card will not be initialised at this time. Initialisation is
    /// deferred until a method is called on the object.
    pub fn new_with_options(
        spi: SPI,
        delayer: DELAYER,
        options: super::spi::AcquireOpts,
    ) -> AsyncSdCard<SPI, DELAYER> {
        AsyncSdCard {
            inner: AsyncSdCardInner {
                spi,
                delayer,
                card_type: None,
                options,
            },
        }
    }

    /// Get a temporary borrow on the underlying SPI device.
    ///
    /// The given closure will be called exactly once, and will be passed a
    /// mutable reference to the underlying SPI object.
    ///
    /// Useful if you need to re-clock the SPI, but does not perform card
    /// initialisation.
    pub fn spi<T, F>(&mut self, func: F) -> T
    where
        F: FnOnce(&mut SPI) -> T,
    {
        func(&mut self.inner.spi)
    }

    /// Return the usable size of this SD card in bytes.
    ///
    /// This will trigger card (re-)initialisation.
    pub async fn num_bytes(&mut self) -> Result<u64, Error> {
        self.inner.check_init().await?;
        self.inner.num_bytes().await
    }

    /// Can this card erase single blocks?
    ///
    /// This will trigger card (re-)initialisation.
    pub async fn erase_single_block_enabled(&mut self) -> Result<bool, Error> {
        self.inner.check_init().await?;
        self.inner.erase_single_block_enabled().await
    }

    /// Mark the card as requiring a reset.
    ///
    /// The next operation will assume the card has been freshly inserted.
    pub fn mark_card_uninit(&mut self) {
        self.inner.card_type = None;
    }

    /// Get the card type.
    ///
    /// This will trigger card (re-)initialisation.
    pub async fn get_card_type(&mut self) -> Option<CardType> {
        self.inner.check_init().await.ok()?;
        self.inner.card_type
    }

    /// Tell the driver the card has been initialised.
    ///
    /// # Safety
    ///
    /// Only do this if the SD Card has actually been initialised. That is, if
    /// you have been through the card initialisation sequence as specified in
    /// the SD Card Specification by sending each appropriate command in turn,
    /// either manually or using another variable of this [`AsyncSdCard`]. The card
    /// must also be of the indicated type. Failure to uphold this will cause
    /// data corruption.
    pub unsafe fn mark_card_as_init(&mut self, card_type: CardType) {
        self.inner.card_type = Some(card_type);
    }
}

impl<SPI, DELAYER> AsyncBlockDevice for AsyncSdCard<SPI, DELAYER>
where
    SPI: embedded_hal_async::spi::SpiDevice<u8>,
    DELAYER: embedded_hal_async::delay::DelayNs,
{
    type Error = Error;

    /// Read one or more blocks, starting at the given block index.
    ///
    /// This will trigger card (re-)initialisation.
    async fn read(
        &mut self,
        blocks: &mut [Block],
        start_block_idx: BlockIdx,
    ) -> Result<(), Self::Error> {
        crate::debug!("Read {} blocks @ {}", blocks.len(), start_block_idx.0);
        self.inner.check_init().await?;
        self.inner.read(blocks, start_block_idx).await
    }

    /// Write one or more blocks, starting at the given block index.
    ///
    /// This will trigger card (re-)initialisation.
    async fn write(
        &mut self,
        blocks: &[Block],
        start_block_idx: BlockIdx,
    ) -> Result<(), Self::Error> {
        crate::debug!("Writing {} blocks @ {}", blocks.len(), start_block_idx.0);
        self.inner.check_init().await?;
        self.inner.write(blocks, start_block_idx).await
    }

    /// Determine how many blocks this device can hold.
    ///
    /// This will trigger card (re-)initialisation.
    async fn num_blocks(&mut self) -> Result<BlockCount, Self::Error> {
        self.inner.check_init().await?;
        self.inner.num_blocks().await
    }
}

/// Inner details for the async SD Card driver.
///
/// All the APIs require `&mut self`.
struct AsyncSdCardInner<SPI, DELAYER>
where
    SPI: embedded_hal_async::spi::SpiDevice<u8>,
    DELAYER: embedded_hal_async::delay::DelayNs,
{
    spi: SPI,
    delayer: DELAYER,
    card_type: Option<CardType>,
    options: super::spi::AcquireOpts,
}

impl<SPI, DELAYER> AsyncSdCardInner<SPI, DELAYER>
where
    SPI: embedded_hal_async::spi::SpiDevice<u8>,
    DELAYER: embedded_hal_async::delay::DelayNs,
{
    /// Read one or more blocks, starting at the given block index.
    async fn read(&mut self, blocks: &mut [Block], start_block_idx: BlockIdx) -> Result<(), Error> {
        let start_idx = match self.card_type {
            Some(CardType::SD1 | CardType::SD2) => start_block_idx.0 * 512,
            Some(CardType::SDHC) => start_block_idx.0,
            None => return Err(Error::CardNotFound),
        };

        if blocks.len() == 1 {
            // Start a single-block read
            self.card_command(CMD17, start_idx).await?;
            self.read_data(&mut blocks[0].contents).await?;
        } else {
            // Start a multi-block read
            self.card_command(CMD18, start_idx).await?;
            for block in blocks.iter_mut() {
                self.read_data(&mut block.contents).await?;
            }
            // Stop the read
            self.card_command(super::CMD12, 0).await?;
        }
        Ok(())
    }

    /// Write one or more blocks, starting at the given block index.
    async fn write(&mut self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Error> {
        let start_idx = match self.card_type {
            Some(CardType::SD1 | CardType::SD2) => start_block_idx.0 * 512,
            Some(CardType::SDHC) => start_block_idx.0,
            None => return Err(Error::CardNotFound),
        };
        if blocks.len() == 1 {
            // Start a single-block write
            self.card_command(CMD24, start_idx).await?;
            self.write_data(DATA_START_BLOCK, &blocks[0].contents)
                .await?;
            self.wait_not_busy(AsyncDelay::new_write()).await?;
            if self.card_command(CMD13, 0).await? != 0x00 {
                return Err(Error::WriteError);
            }
            if self.read_byte().await? != 0x00 {
                return Err(Error::WriteError);
            }
        } else {
            // > It is recommended using this command preceding CMD25, some of the cards will be faster for Multiple
            // > Write Blocks operation. Note that the host should send ACMD23 just before WRITE command if the host
            // > wants to use the pre-erased feature
            self.card_acmd(ACMD23, blocks.len() as u32).await?;
            // wait for card to be ready before sending the next command
            self.wait_not_busy(AsyncDelay::new_write()).await?;

            // Start a multi-block write
            self.card_command(CMD25, start_idx).await?;
            for block in blocks.iter() {
                self.wait_not_busy(AsyncDelay::new_write()).await?;
                self.write_data(WRITE_MULTIPLE_TOKEN, &block.contents)
                    .await?;
            }
            // Stop the write
            self.wait_not_busy(AsyncDelay::new_write()).await?;
            self.write_byte(STOP_TRAN_TOKEN).await?;
        }
        Ok(())
    }

    /// Determine how many blocks this device can hold.
    async fn num_blocks(&mut self) -> Result<BlockCount, Error> {
        let csd = self.read_csd().await?;
        crate::debug!("CSD: {:?}", csd);
        Ok(BlockCount(csd.card_capacity_blocks()))
    }

    /// Return the usable size of this SD card in bytes.
    async fn num_bytes(&mut self) -> Result<u64, Error> {
        let csd = self.read_csd().await?;
        crate::debug!("CSD: {:?}", csd);
        Ok(csd.card_capacity_bytes())
    }

    /// Can this card erase single blocks?
    pub async fn erase_single_block_enabled(&mut self) -> Result<bool, Error> {
        let csd = self.read_csd().await?;
        Ok(csd.erase_single_block_enabled())
    }

    /// Read the 'card specific data' block.
    async fn read_csd(&mut self) -> Result<csd::Csd, Error> {
        let mut csd_raw: [u8; 16] = [0; 16];
        match self.card_type {
            Some(CardType::SD1) => {
                if self.card_command(CMD9, 0).await? != 0 {
                    return Err(Error::RegisterReadError);
                }
                self.read_data(&mut csd_raw).await?;
                Ok(csd::Csd::V1(csd::CsdV1::from_be_bytes(&csd_raw)))
            }
            Some(CardType::SD2 | CardType::SDHC) => {
                if self.card_command(CMD9, 0).await? != 0 {
                    return Err(Error::RegisterReadError);
                }
                self.read_data(&mut csd_raw).await?;
                Ok(csd::Csd::V2(csd::CsdV2::from_be_bytes(&csd_raw)))
            }
            None => Err(Error::CardNotFound),
        }
    }

    /// Read an arbitrary number of bytes from the card using the SD Card
    /// protocol and an optional CRC. Always fills the given buffer, so make
    /// sure it's the right size.
    async fn read_data(&mut self, buffer: &mut [u8]) -> Result<(), Error> {
        // Get first non-FF byte.
        let mut delay = AsyncDelay::new_read();
        let status = loop {
            let s = self.read_byte().await?;
            if s != 0xFF {
                break s;
            }
            delay
                .delay(&mut self.delayer, Error::TimeoutReadBuffer)
                .await?;
        };
        if status != DATA_START_BLOCK {
            return Err(Error::ReadError);
        }

        buffer.fill(0xFF);
        self.transfer_bytes(buffer).await?;

        // These two bytes are always sent. They are either a valid CRC, or
        // junk, depending on whether CRC mode was enabled.
        let mut crc_bytes = [0xFF; 2];
        self.transfer_bytes(&mut crc_bytes).await?;
        if self.options.use_crc {
            let crc = u16::from_be_bytes(crc_bytes);
            let calc_crc = crc16(buffer);
            if crc != calc_crc {
                return Err(Error::CrcError(crc, calc_crc));
            }
        }

        Ok(())
    }

    /// Write an arbitrary number of bytes to the card using the SD protocol and
    /// an optional CRC.
    async fn write_data(&mut self, token: u8, buffer: &[u8]) -> Result<(), Error> {
        self.write_byte(token).await?;
        self.write_bytes(buffer).await?;
        let crc_bytes = if self.options.use_crc {
            crc16(buffer).to_be_bytes()
        } else {
            [0xFF, 0xFF]
        };
        // These two bytes are always sent. They are either a valid CRC, or
        // junk, depending on whether CRC mode was enabled.
        self.write_bytes(&crc_bytes).await?;

        let status = self.read_byte().await?;
        if (status & DATA_RES_MASK) != DATA_RES_ACCEPTED {
            Err(Error::WriteError)
        } else {
            Ok(())
        }
    }

    /// Check the card is initialised.
    async fn check_init(&mut self) -> Result<(), Error> {
        if self.card_type.is_none() {
            // If we don't know what the card type is, try and initialise the
            // card. This will tell us what type of card it is.
            self.acquire().await
        } else {
            Ok(())
        }
    }

    /// Initializes the card into a known state (or at least tries to).
    async fn acquire(&mut self) -> Result<(), Error> {
        crate::debug!("acquiring card with opts: {:?}", self.options);
        // Assume it hasn't worked
        let mut card_type;
        crate::trace!("Reset card..");
        // Enter SPI mode.
        let mut delay = AsyncDelay::new(self.options.acquire_retries);
        for _attempts in 1.. {
            crate::trace!("Enter SPI mode, attempt: {}..", _attempts);
            match self.card_command(CMD0, 0).await {
                Err(Error::TimeoutCommand(0)) => {
                    // Try again?
                    crate::warn!("Timed out, trying again..");
                    // Try flushing the card as done here: https://github.com/greiman/SdFat/blob/master/src/SdCard/SdSpiCard.cpp#L170,
                    // https://github.com/rust-embedded-community/embedded-sdmmc-rs/pull/65#issuecomment-1270709448
                    for _ in 0..0xFF {
                        self.write_byte(0xFF).await?;
                    }
                }
                Err(e) => {
                    let _ = self.read_byte().await;
                    return Err(e);
                }
                Ok(R1_IDLE_STATE) => {
                    break;
                }
                Ok(_r) => {
                    // Try again
                    crate::trace!("Got response: {:x}, trying again..", _r);
                }
            }

            delay.delay(&mut self.delayer, Error::CardNotFound).await?;
        }
        // Enable CRC
        crate::debug!("Enable CRC: {}", self.options.use_crc);
        // "The SPI interface is initialized in the CRC OFF mode in default"
        // -- SD Part 1 Physical Layer Specification v9.00, Section 7.2.2 Bus Transfer Protection
        if self.options.use_crc && self.card_command(CMD59, 1).await? != R1_IDLE_STATE {
            let _ = self.read_byte().await;
            return Err(Error::CantEnableCRC);
        }
        // Check card version
        let mut delay = AsyncDelay::new_command();
        let arg = loop {
            if self.card_command(CMD8, 0x1AA).await? == (R1_ILLEGAL_COMMAND | R1_IDLE_STATE) {
                card_type = CardType::SD1;
                break 0;
            }
            let mut buffer = [0xFF; 4];
            self.transfer_bytes(&mut buffer).await?;
            let status = buffer[3];
            if status == 0xAA {
                card_type = CardType::SD2;
                break 0x4000_0000;
            }
            delay
                .delay(&mut self.delayer, Error::TimeoutCommand(CMD8))
                .await?;
        };

        let mut delay = AsyncDelay::new_command();
        while self.card_acmd(ACMD41, arg).await? != R1_READY_STATE {
            delay
                .delay(&mut self.delayer, Error::TimeoutACommand(ACMD41))
                .await?;
        }

        if card_type == CardType::SD2 {
            if self.card_command(CMD58, 0).await? != 0 {
                let _ = self.read_byte().await;
                return Err(Error::Cmd58Error);
            }
            let mut buffer = [0xFF; 4];
            self.transfer_bytes(&mut buffer).await?;
            if (buffer[0] & 0xC0) == 0xC0 {
                card_type = CardType::SDHC;
            }
            // Ignore the other three bytes
        }
        crate::debug!("Card version: {:?}", card_type);
        self.card_type = Some(card_type);
        let _ = self.read_byte().await;
        Ok(())
    }

    /// Perform an application-specific command.
    async fn card_acmd(&mut self, command: u8, arg: u32) -> Result<u8, Error> {
        self.card_command(CMD55, 0).await?;
        self.card_command(command, arg).await
    }

    /// Perform a command.
    async fn card_command(&mut self, command: u8, arg: u32) -> Result<u8, Error> {
        if command != CMD0 && command != CMD12 {
            self.wait_not_busy(AsyncDelay::new_command()).await?;
        }

        let mut buf = [
            0x40 | command,
            (arg >> 24) as u8,
            (arg >> 16) as u8,
            (arg >> 8) as u8,
            arg as u8,
            0,
        ];
        buf[5] = crc7(&buf[0..5]);

        self.write_bytes(&buf).await?;

        // skip stuff byte for stop read
        if command == CMD12 {
            let _result = self.read_byte().await?;
        }

        let mut delay = AsyncDelay::new_command();
        loop {
            let result = self.read_byte().await?;
            if (result & 0x80) == ERROR_OK {
                return Ok(result);
            }
            delay
                .delay(&mut self.delayer, Error::TimeoutCommand(command))
                .await?;
        }
    }

    /// Receive a byte from the SPI bus by clocking out an 0xFF byte.
    async fn read_byte(&mut self) -> Result<u8, Error> {
        self.transfer_byte(0xFF).await
    }

    /// Send a byte over the SPI bus and ignore what comes back.
    async fn write_byte(&mut self, out: u8) -> Result<(), Error> {
        let _ = self.transfer_byte(out).await?;
        Ok(())
    }

    /// Send one byte and receive one byte over the SPI bus.
    async fn transfer_byte(&mut self, out: u8) -> Result<u8, Error> {
        let mut read_buf = [0u8; 1];
        self.spi
            .transfer(&mut read_buf, &[out])
            .await
            .map_err(|_| Error::Transport)?;
        Ok(read_buf[0])
    }

    /// Send multiple bytes and ignore what comes back over the SPI bus.
    async fn write_bytes(&mut self, out: &[u8]) -> Result<(), Error> {
        self.spi.write(out).await.map_err(|_e| Error::Transport)?;
        Ok(())
    }

    /// Send multiple bytes and replace them with what comes back over the SPI bus.
    async fn transfer_bytes(&mut self, in_out: &mut [u8]) -> Result<(), Error> {
        self.spi
            .transfer_in_place(in_out)
            .await
            .map_err(|_e| Error::Transport)?;
        Ok(())
    }

    /// Spin until the card returns 0xFF, or we spin too many times and
    /// timeout.
    async fn wait_not_busy(&mut self, mut delay: AsyncDelay) -> Result<(), Error> {
        loop {
            let s = self.read_byte().await?;
            if s == 0xFF {
                break;
            }
            delay
                .delay(&mut self.delayer, Error::TimeoutWaitNotBusy)
                .await?;
        }
        Ok(())
    }
}

/// This is an object you can use to busy-wait with a timeout, using async delays.
///
/// Will let you call `delay` up to `max_retries` times before `delay` returns
/// an error.
struct AsyncDelay {
    retries_left: u32,
}

impl AsyncDelay {
    /// The default number of retries for a read operation.
    ///
    /// At ~10us each this is ~100ms.
    ///
    /// See `Part1_Physical_Layer_Simplified_Specification_Ver9.00-1.pdf` Section 4.6.2.1
    pub const DEFAULT_READ_RETRIES: u32 = 10_000;

    /// The default number of retries for a write operation.
    ///
    /// At ~10us each this is ~500ms.
    ///
    /// See `Part1_Physical_Layer_Simplified_Specification_Ver9.00-1.pdf` Section 4.6.2.2
    pub const DEFAULT_WRITE_RETRIES: u32 = 50_000;

    /// The default number of retries for a control command.
    ///
    /// At ~10us each this is ~100ms.
    ///
    /// No value is given in the specification, so we pick the same as the read timeout.
    pub const DEFAULT_COMMAND_RETRIES: u32 = 10_000;

    /// Create a new AsyncDelay object with the given maximum number of retries.
    fn new(max_retries: u32) -> AsyncDelay {
        AsyncDelay {
            retries_left: max_retries,
        }
    }

    /// Create a new AsyncDelay object with the maximum number of retries for a read operation.
    fn new_read() -> AsyncDelay {
        AsyncDelay::new(Self::DEFAULT_READ_RETRIES)
    }

    /// Create a new AsyncDelay object with the maximum number of retries for a write operation.
    fn new_write() -> AsyncDelay {
        AsyncDelay::new(Self::DEFAULT_WRITE_RETRIES)
    }

    /// Create a new AsyncDelay object with the maximum number of retries for a command operation.
    fn new_command() -> AsyncDelay {
        AsyncDelay::new(Self::DEFAULT_COMMAND_RETRIES)
    }

    /// Wait for a while.
    ///
    /// Checks the retry counter first, and if we hit the max retry limit, the
    /// value `err` is returned. Otherwise we wait for 10us and then return
    /// `Ok(())`.
    async fn delay<T>(&mut self, delayer: &mut T, err: Error) -> Result<(), Error>
    where
        T: embedded_hal_async::delay::DelayNs,
    {
        if self.retries_left == 0 {
            Err(err)
        } else {
            delayer.delay_us(10).await;
            self.retries_left -= 1;
            Ok(())
        }
    }
}

// Re-export the error and card types from the sync module since they are identical
pub use super::spi::{AcquireOpts, CardType, Error};
