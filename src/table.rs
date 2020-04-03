// Copyright 2015-2020 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::convert::TryInto;
use std::os::unix::fs::FileExt;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU64, Ordering};
use crate::{
	error::Result,
	column::ColId,
	log::{LogOverlays, LogReader, LogWriter},
	display::hex,
};

pub const KEY_LEN: usize = 32;
const MAX_ENTRY_SIZE: usize = 65534;

const OFFSET_BITS: u8 = 32;
const OFFSET_MASK: u64 = (1u64 << OFFSET_BITS) - 1;
const TOMBSTONE: &[u8] = &[0xff, 0xff];
const MULTIPART: &[u8] = &[0xff, 0xfe];

pub type Key = [u8; KEY_LEN];
pub type Value = Vec<u8>;

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct TableId(u16);

impl TableId {
	pub fn new(col: ColId, size_tier: u8) -> TableId {
		TableId(((col as u16) << 8) | size_tier as u16)
	}

	pub fn from_u16(id: u16) -> TableId {
		TableId(id)
	}

	pub fn col(&self) -> ColId {
		(self.0 >> 8) as ColId
	}

	pub fn size_tier(&self) -> u8 {
		(self.0 & 0xff) as u8
	}

	pub fn file_name(&self) -> String {
		format!("table_{:02}_{}", self.col(), self.size_tier())
	}

	pub fn as_u16(&self) -> u16 {
		self.0
	}
}

impl std::fmt::Display for TableId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "table {:02}_{}", self.col(), self.size_tier())
	}
}

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct Address(u64);

impl Address {
	pub fn new(offset: u64, size_tier: u8) -> Address {
		Address(((size_tier as u64) << OFFSET_BITS) | offset)
	}

	pub fn from_u64(a: u64) -> Address {
		Address(a)
	}

	pub fn offset(&self) -> u64 {
		self.0 & OFFSET_MASK
	}

	pub fn size_tier(&self) -> u8 {
		((self.0 >> OFFSET_BITS) & 0xff) as u8
	}

	pub fn as_u64(&self) -> u64 {
		self.0
	}
}

impl std::fmt::Display for Address {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "addr {:02}:{}", self.size_tier(), self.offset())
	}
}

pub struct ValueTable {
	pub id: TableId,
	pub entry_size: u16,
	file: std::fs::File,
	capacity: u64,
	filled: AtomicU64,
	last_removed: AtomicU64,
	multipart: bool,
}

impl ValueTable {
	pub fn open(path: &std::path::Path, id: TableId, entry_size: Option<u16>) -> Result<ValueTable> {
		let (multipart, entry_size) = match entry_size {
			Some(s) => (false, s),
			None => (true, 4096),
		};
		assert!(entry_size >= 64);
		assert!(entry_size <= MAX_ENTRY_SIZE as u16);
		// TODO: posix_fadvise
		let mut path: std::path::PathBuf = path.into();
		path.push(id.file_name());

		let file = std::fs::OpenOptions::new().create(true).read(true).write(true).open(path.as_path())?;
		let mut file_len = file.metadata()?.len();
		if file_len == 0 {
			// Prealocate a single entry that also
			file.set_len(entry_size as u64)?;
			file_len = entry_size as u64;
		}

		let capacity = file_len / entry_size as u64;

		let mut header: [u8; 16] = Default::default();
		file.read_exact_at(&mut header, 0)?;
		let last_removed = u64::from_le_bytes(header[0..8].try_into().unwrap());
		let mut filled = u64::from_le_bytes(header[8..16].try_into().unwrap());
		if filled == 0 {
			filled = 1;
		}
		log::debug!(target: "parity-db", "Opened value table {} with {} entries", id, filled);
		Ok(ValueTable {
			id,
			entry_size,
			file,
			capacity,
			filled: AtomicU64::new(filled),
			last_removed: AtomicU64::new(last_removed),
			multipart,
		})
	}

	pub fn value_size(&self) -> u16 {
		self.entry_size - KEY_LEN as u16
	}

	fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
		Ok(self.file.read_exact_at(buf, offset)?)
	}

	fn write_at(&self, buf: &[u8], offset: u64) -> Result<()> {
		Ok(self.file.write_all_at(buf, offset)?)
	}

	fn grow(&mut self) -> Result<()> {
		self.capacity += (256 * 1024) / self.entry_size as u64;
		self.file.set_len(self.capacity * self.entry_size as u64)?;
		Ok(())
	}

	pub fn get(&self, key: &Key, mut index: u64, log: &LogOverlays) -> Result<Option<Value>> {
		let mut buf: [u8; MAX_ENTRY_SIZE] = unsafe { MaybeUninit::uninit().assume_init() };
		let mut result = Vec::new();

		let mut part = 0;
		loop {
			let buf = if let Some(overlay) = log.value(self.id, index) {
				overlay
			} else {
				self.read_at(&mut buf[0 .. self.entry_size as usize], index * self.entry_size as u64)?;
				&buf[0 .. self.entry_size as usize]
			};

			if &buf[0..2] == TOMBSTONE {
				return Ok(None);
			}

			let (content_offset, content_len, next) = if &buf[0..2] == MULTIPART {
				let next = u64::from_le_bytes(buf[2..10].try_into().unwrap());
				(10, self.entry_size as usize - 10, next)
			} else {
				let size: u16 = u16::from_le_bytes(buf[0..2].try_into().unwrap());
				(2, size as usize, 0)
			};

			if part == 0 {
				if key[2..] != buf[content_offset..content_offset + 30] {
					log::warn!(
						target: "parity-db",
						"{}: Key mismatch at {}. Expected {}, got {}",
						self.id,
						index,
						hex(&key[2..]),
						hex(&buf[content_offset..content_offset + 30]),
					);
					return Ok(None);
				}
				result.extend_from_slice(&buf[content_offset + 30 .. content_offset + content_len]);
			} else {
				result.extend_from_slice(&buf[content_offset .. content_offset + content_len]);
			}
			if next == 0 {
				break;
			}
			part += 1;
			index = next;
		}
		Ok(Some(result))
	}

	pub fn has_key_at(&self, index: u64, key: &Key, log: &LogWriter) -> Result<bool> {
		Ok(match self.partial_key_at(index, log)? {
			Some(existing_key) => &existing_key[2..] == &key [2..],
			None => false,
		})
	}

	pub fn partial_key_at(&self, index: u64, log: &LogWriter) -> Result<Option<Key>> {
		let mut buf = [0u8; 40];
		let mut result = Key::default();
		let buf = if let Some(overlay) = log.value_overlay_at(self.id, index) {
			overlay
		} else {
			self.read_at(&mut buf[0 .. 40], index * self.entry_size as u64)?;
			&buf[0 .. 40]
		};
		if &buf[0..2] == TOMBSTONE {
			return Ok(None);
		}
		if &buf[0..2] == MULTIPART {
			result[2..].copy_from_slice(&buf[10..40]);
		} else {
			result[2..].copy_from_slice(&buf[2..32]);
		}
		Ok(Some(result))
	}

	pub fn read_next_free(&self, index: u64, log: &LogWriter) -> Result<u64> {
		if let Some(val) = log.value_overlay_at(self.id, index) {
			let next = u64::from_le_bytes(val[2..10].try_into().unwrap());
			return Ok(next);
		}
		let mut buf: [u8; 10] = unsafe { MaybeUninit::uninit().assume_init() };
		self.read_at(&mut buf, index * self.entry_size as u64)?;
		let next = u64::from_le_bytes(buf[2..10].try_into().unwrap());
		return Ok(next);
	}

	pub fn read_next_part(&self, index: u64, log: &LogWriter) -> Result<Option<u64>> {
		if let Some(val) = log.value_overlay_at(self.id, index) {
			if &val[0..2] == MULTIPART {
				let next = u64::from_le_bytes(val[2..10].try_into().unwrap());
				return Ok(Some(next));
			}
			return Ok(None);
		}
		let mut buf: [u8; 10] = unsafe { MaybeUninit::uninit().assume_init() };
		self.read_at(&mut buf, index * self.entry_size as u64)?;
		if &buf[0..2] == MULTIPART {
			let next = u64::from_le_bytes(buf[2..10].try_into().unwrap());
			return Ok(Some(next));
		}
		return Ok(None);
	}

	pub fn next_free(&self, log: &mut LogWriter) -> Result<u64> {
		let filled = self.filled.load(Ordering::Relaxed);
		let last_removed = self.last_removed.load(Ordering::Relaxed);
		let index = if last_removed != 0 {
			let next_removed = self.read_next_free(last_removed, log)?;
			log::trace!(
				target: "parity-db",
				"{}: Inserting into removed slot {}",
				self.id,
				last_removed,
			);
			self.last_removed.store(next_removed, Ordering::Relaxed);
			last_removed
		} else {
			log::trace!(
				target: "parity-db",
				"{}: Inserting into new slot {}",
				self.id,
				filled + 1,
			);
			self.filled.store(filled + 1, Ordering::Relaxed);
			filled + 1
		};
		Ok(index)
	}

	fn overwrite_chain(&self, key: &Key, value: &[u8], log: &mut LogWriter, at: Option<u64>) -> Result<u64> {
		let mut remainder = value.len() + 30; // Prefix with key
		let mut offset = 0;
		let mut start = 0;
		assert!(self.multipart || value.len() <= self.value_size() as usize);
		let (mut index, mut follow) = match at {
			Some(index) => (index, true),
			None => (self.next_free(log)?, false)
		};
		loop {
			if start == 0 {
				start = index;
			}

			let mut next_index = 0;
			if follow {
				// check existing link
				match self.read_next_part(index, log)? {
					Some(next) => {
						next_index = next;
					}
					None => {
						follow = false;
					}
				}
			}
			log::trace!(
				target: "parity-db",
				"{}: Writing slot {}: {}",
				self.id,
				index,
				hex(key),
			);
			let mut buf: [u8; MAX_ENTRY_SIZE] = unsafe { MaybeUninit::uninit().assume_init() };
			let free_space = self.entry_size as usize - 2;
			let (target_offset, value_len) = if remainder > free_space {
				if !follow {
					next_index = self.next_free(log)?
				}
				buf[0..2].copy_from_slice(&MULTIPART);
				buf[2..10].copy_from_slice(&(next_index as u64).to_le_bytes());
				(10, free_space - 8)
			} else {
				buf[0..2].copy_from_slice(&(remainder as u16).to_le_bytes());
				(2, remainder)
			};
			if offset == 0 {
				buf[target_offset..target_offset + 30].copy_from_slice(&key[2..]);
				buf[target_offset + 30.. target_offset + value_len].copy_from_slice(&value[offset..offset+value_len-30]);
				offset += value_len - 30;
			} else {
				buf[target_offset..target_offset+value_len].copy_from_slice(&value[offset..offset+value_len]);
				offset += value_len;
			}
			/*
			log::trace!(
				target: "parity-db",
				"SLOT {} val bytes, slot {} bytes: {}",
				value.len(),
				target_offset + value_len,
				hex(&buf[0..target_offset + value_len]),
			);*/
			log.insert_value(self.id, index, buf[0..target_offset+value_len].to_vec());
			remainder -= value_len;
			index = next_index;
			if remainder == 0 {
				if index != 0 {
					// End of new entry. Clear the remaining tail and exit
					self.clear_chain(index, log)?;
				}
				break;
			}
		}

		Ok(start)
	}

	fn clear_chain(&self, mut index: u64, log: &mut LogWriter) -> Result<()> {
		loop {
			match self.read_next_part(index, log)? {
				Some(next) => {
					self.clear_slot(index, log)?;
					index = next;
				}
				None => {
					self.clear_slot(index, log)?;
					return Ok(());
				}
			}
		}
	}

	fn clear_slot(&self, index: u64, log: &mut LogWriter) -> Result<()> {
		let last_removed = self.last_removed.load(Ordering::Relaxed);
		log::trace!(
			target: "parity-db",
			"{}: Freeing slot {}",
			self.id,
			index,
		);
		let mut buf = [0u8; 10];
		&buf[0..2].copy_from_slice(TOMBSTONE);
		&buf[2..10].copy_from_slice(&last_removed.to_le_bytes());

		log.insert_value(self.id, index, buf.to_vec());
		self.last_removed.store(index, Ordering::Relaxed);
		Ok(())
	}

	pub fn write_insert_plan(&self, key: &Key, value: &[u8], log: &mut LogWriter) -> Result<u64> {
		self.overwrite_chain(key, value, log, None)
	}

	pub fn write_replace_plan(&self, index: u64, key: &Key, value: &[u8], log: &mut LogWriter) -> Result<()> {
		self.overwrite_chain(key, value, log, Some(index))?;
		Ok(())
	}

	pub fn write_remove_plan(&self, index: u64, log: &mut LogWriter) -> Result<()> {
		if self.multipart {
			self.clear_chain(index, log)?;
		} else {
			self.clear_slot(index, log)?;
		}
		Ok(())
	}

	pub fn enact_plan(&mut self, index: u64, log: &mut LogReader) -> Result<()> {
		while index > self.capacity {
			self.grow()?;
		}

		let mut buf: [u8; MAX_ENTRY_SIZE] = unsafe { MaybeUninit::uninit().assume_init() };
		log.read(&mut buf[0..2])?;
		if &buf[0..2] == TOMBSTONE {
			log.read(&mut buf[2..10])?;
			self.write_at(&buf[0..10], index * (self.entry_size as u64))?;
			log::trace!(target: "parity-db", "{}: Enacted tombstone in slot {}", self.id, index);
		}
		else if &buf[0..2] == MULTIPART {
				let entry_size = self.entry_size as usize;
				log.read(&mut buf[2..entry_size])?;
				self.write_at(&buf[0..entry_size], index * (entry_size as u64))?;
				log::trace!(target: "parity-db", "{}: Enacted multipart in slot {}", self.id, index);
		} else {
			let len: u16 = u16::from_le_bytes(buf[0..2].try_into().unwrap());
			log.read(&mut buf[2..2+len as usize])?;
			self.write_at(&buf[0..(2 + len as usize)], index * (self.entry_size as u64))?;
			log::trace!(target: "parity-db", "{}: Enacted {}: {}, {} bytes", self.id, index, hex(&buf[2..32]), len);
		}
		Ok(())
	}

	pub fn complete_plan(&mut self) -> Result<()> {
		let mut buf = [0u8; 16];
		let last_removed = self.last_removed.load(Ordering::Relaxed);
		let filled = self.filled.load(Ordering::Relaxed);
		buf[0..8].copy_from_slice(&last_removed.to_le_bytes());
		buf[8..16].copy_from_slice(&filled.to_le_bytes());
		self.write_at(&buf, 0)?;
		Ok(())
	}
}

