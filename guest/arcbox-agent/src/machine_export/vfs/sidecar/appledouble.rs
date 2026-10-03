//! The AppleDouble container macOS writes for a file's extended attributes
//! when the filesystem has none of its own, and the one the export answers
//! with.
//!
//! On such a filesystem macOS keeps a file's attributes in a sibling
//! `._<name>`: an AppleDouble v2 file (RFC 1740; Apple's `copyfile.c` and
//! XNU's `vfs_xattr.c` are the writers), big-endian throughout. A header
//! and an entry table name regions of the file; Apple's writers always
//! emit two entries, Finder Info (type 9) and the resource fork (type 2).
//! The Finder Info entry is 32 bytes of Finder Info followed by Apple's
//! extension: an `ATTR` header listing the ordinary attributes by name,
//! each pointing into a data area that lies between the attribute entries
//! and the resource fork.
//!
//! Parsing accepts what XNU's reader accepts: one to fifteen entries in
//! any order, attributes only when the Finder Info entry sits at Apple's
//! offset and carries the `ATTR` magic, and a resource fork that is
//! neither empty nor the placeholder XNU writes for "no fork". An image
//! that ends before a region its tables declare is *incomplete* rather
//! than invalid: the Mac writes a sidecar in pieces, and the whole only
//! exists once the last piece lands. Serialization produces the layout
//! `copyfile(3)` produces — header, Finder Info, `ATTR` header and
//! entries, attribute data, resource fork — without the 4 KiB of slack
//! XNU reserves for growth, so an image is exactly as long as its content.

use std::fmt;

/// Finder Info is always 32 bytes; all zero means "none".
pub const FINDER_INFO_LEN: usize = 32;
/// XNU reads at most this many attribute entries from one `ATTR` header.
pub const MAX_ATTRS: usize = 256;
/// An attribute name's length limit: the entry's `namelen` is one byte
/// and counts the terminating NUL.
pub const MAX_NAME_LEN: usize = 254;

const MAGIC: u32 = 0x0005_1607;
const VERSION: u32 = 0x0002_0000;
const FILLER: &[u8; 16] = b"Mac OS X        ";
const HEADER_LEN: usize = 26;
const ENTRY_LEN: usize = 12;
const MAX_ENTRIES: usize = 15;
const TYPE_RESOURCE_FORK: u32 = 2;
const TYPE_FINDER_INFO: u32 = 9;
/// Where Apple's writers put the Finder Info: right after two entries.
const FINDER_INFO_OFFSET: usize = HEADER_LEN + 2 * ENTRY_LEN;
/// The `ATTR` header follows the Finder Info and two bytes of padding.
const ATTR_HEADER_OFFSET: usize = FINDER_INFO_OFFSET + FINDER_INFO_LEN + 2;
const ATTR_MAGIC: u32 = 0x4154_5452;
const ATTR_HEADER_LEN: usize = 36;
const ATTR_ENTRIES_OFFSET: usize = ATTR_HEADER_OFFSET + ATTR_HEADER_LEN;
/// The fixed part of an attribute entry: offset, length, flags, namelen.
const ATTR_ENTRY_FIXED_LEN: usize = 11;
/// What XNU writes 16 bytes into a resource fork it had to create empty.
const EMPTY_FORK_TAG: &[u8; 47] = b"This resource fork intentionally left blank   \0";
const EMPTY_FORK_TAG_OFFSET: usize = 16;

/// One ordinary extended attribute: the name without its NUL, and the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

/// The content of a sidecar: what the Mac sees as a file's attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppleDouble {
    pub finder_info: Option<[u8; FINDER_INFO_LEN]>,
    pub resource_fork: Option<Vec<u8>>,
    pub attrs: Vec<Attr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The image ends before a region its tables declare: the writer has
    /// not finished.
    Incomplete,
    /// Not an AppleDouble v2 file, or one whose tables contradict each other.
    Invalid(&'static str),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete => f.write_str("the image ends before its declared content"),
            Self::Invalid(why) => write!(f, "not a usable AppleDouble file: {why}"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    type_: u32,
    offset: usize,
    length: usize,
}

impl Entry {
    const fn end(self) -> usize {
        self.offset + self.length
    }
}

/// Length of an attribute entry with a `namelen`-byte name (NUL included),
/// padded to four bytes as XNU's `ATTR_ENTRY_LENGTH` does.
const fn entry_len(namelen: usize) -> usize {
    (ATTR_ENTRY_FIXED_LEN + namelen + 3) & !3
}

/// `N` bytes at `at`, or `Incomplete` when the image ends first.
fn field<const N: usize>(image: &[u8], at: usize) -> Result<[u8; N], ParseError> {
    image
        .get(at..at.saturating_add(N))
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(ParseError::Incomplete)
}

fn be16(image: &[u8], at: usize) -> Result<usize, ParseError> {
    field::<2>(image, at).map(|b| usize::from(u16::from_be_bytes(b)))
}

fn be32(image: &[u8], at: usize) -> Result<usize, ParseError> {
    // `usize` is at least 32 bits on every target the agent builds for.
    field::<4>(image, at).map(|b| u32::from_be_bytes(b) as usize)
}

fn put32(out: &mut Vec<u8>, value: usize) {
    out.extend_from_slice(&(value as u32).to_be_bytes());
}

impl AppleDouble {
    /// Whether `image` begins like an AppleDouble file — or, shorter than
    /// the magic, could still become one.
    pub fn has_magic(image: &[u8]) -> bool {
        let seen = image.len().min(4);
        MAGIC.to_be_bytes()[..seen] == image[..seen]
    }

    /// Whether the sidecar carries nothing: the Mac removes such a file.
    pub fn is_empty(&self) -> bool {
        self.finder_info.is_none() && self.resource_fork.is_none() && self.attrs.is_empty()
    }

    pub fn parse(image: &[u8]) -> Result<Self, ParseError> {
        if be32(image, 0)? != MAGIC as usize {
            return Err(ParseError::Invalid("bad magic"));
        }
        if be32(image, 4)? != VERSION as usize {
            return Err(ParseError::Invalid("not version 2"));
        }
        let count = be16(image, 24)?;
        if !(1..=MAX_ENTRIES).contains(&count) {
            return Err(ParseError::Invalid("entry count out of range"));
        }
        let table_end = HEADER_LEN + count * ENTRY_LEN;
        let mut entries: Vec<Entry> = Vec::with_capacity(count);
        for i in 0..count {
            let at = HEADER_LEN + i * ENTRY_LEN;
            let entry = Entry {
                type_: be32(image, at)? as u32,
                offset: be32(image, at + 4)?,
                length: be32(image, at + 8)?,
            };
            if entry.offset < table_end {
                return Err(ParseError::Invalid("entry data inside the header"));
            }
            if entries
                .iter()
                .any(|other| entry.end() > other.offset && other.end() > entry.offset)
            {
                return Err(ParseError::Invalid("entries overlap"));
            }
            entries.push(entry);
        }
        // The whole exists only once every declared region is present.
        if entries.iter().map(|entry| entry.end()).max() > Some(image.len()) {
            return Err(ParseError::Incomplete);
        }

        let mut parsed = Self::default();
        if let Some(finder) = entries.iter().find(|e| e.type_ == TYPE_FINDER_INFO) {
            if finder.length >= FINDER_INFO_LEN {
                let info: [u8; FINDER_INFO_LEN] = field(image, finder.offset)?;
                if info.iter().any(|&b| b != 0) {
                    parsed.finder_info = Some(info);
                }
            }
            if finder.offset == FINDER_INFO_OFFSET
                && finder.end() >= ATTR_ENTRIES_OFFSET
                && be32(image, ATTR_HEADER_OFFSET)? == ATTR_MAGIC as usize
            {
                parsed.attrs = parse_attrs(image, finder.end())?;
            }
        }
        if let Some(fork) = entries.iter().find(|e| e.type_ == TYPE_RESOURCE_FORK) {
            let data = &image[fork.offset..fork.end()];
            let blank = data
                .get(EMPTY_FORK_TAG_OFFSET..EMPTY_FORK_TAG_OFFSET + EMPTY_FORK_TAG.len())
                .is_some_and(|tag| tag == EMPTY_FORK_TAG);
            if !data.is_empty() && !blank {
                parsed.resource_fork = Some(data.to_vec());
            }
        }
        Ok(parsed)
    }

    /// The image in Apple's layout.
    ///
    /// The caller keeps the content within the format's limits: at most
    /// [`MAX_ATTRS`] attributes, names of at most [`MAX_NAME_LEN`] bytes,
    /// and a total under 4 GiB, which every source feeding this type
    /// guarantees (Linux caps an inode's attribute at 64 KiB, the export
    /// caps a sidecar image).
    pub fn to_bytes(&self) -> Vec<u8> {
        debug_assert!(self.attrs.len() <= MAX_ATTRS);
        let entries_len: usize = self
            .attrs
            .iter()
            .map(|attr| entry_len(attr.name.len() + 1))
            .sum();
        let data_start = ATTR_ENTRIES_OFFSET + entries_len;
        let data_length: usize = self.attrs.iter().map(|attr| attr.value.len()).sum();
        let fork_offset = data_start + data_length;
        let fork = self.resource_fork.as_deref().unwrap_or_default();

        let mut out = Vec::with_capacity(fork_offset + fork.len());
        out.extend_from_slice(&MAGIC.to_be_bytes());
        out.extend_from_slice(&VERSION.to_be_bytes());
        out.extend_from_slice(FILLER);
        out.extend_from_slice(&2u16.to_be_bytes());
        // Finder Info, spanning the attribute area as Apple's writers have it.
        out.extend_from_slice(&TYPE_FINDER_INFO.to_be_bytes());
        put32(&mut out, FINDER_INFO_OFFSET);
        put32(&mut out, fork_offset - FINDER_INFO_OFFSET);
        out.extend_from_slice(&TYPE_RESOURCE_FORK.to_be_bytes());
        put32(&mut out, fork_offset);
        put32(&mut out, fork.len());
        out.extend_from_slice(&self.finder_info.unwrap_or([0; FINDER_INFO_LEN]));
        out.extend_from_slice(&[0; 2]);
        // The `ATTR` header: magic, debug tag, total size, data start and
        // length, three reserved words, flags, count.
        out.extend_from_slice(&ATTR_MAGIC.to_be_bytes());
        put32(&mut out, 0);
        put32(&mut out, fork_offset);
        put32(&mut out, data_start);
        put32(&mut out, data_length);
        out.extend_from_slice(&[0; 12]);
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&(self.attrs.len() as u16).to_be_bytes());
        let mut value_offset = data_start;
        for attr in &self.attrs {
            debug_assert!(attr.name.len() <= MAX_NAME_LEN);
            let namelen = attr.name.len() + 1;
            put32(&mut out, value_offset);
            put32(&mut out, attr.value.len());
            out.extend_from_slice(&0u16.to_be_bytes());
            out.push(namelen as u8);
            out.extend_from_slice(&attr.name);
            out.push(0);
            out.resize(
                out.len() + entry_len(namelen) - ATTR_ENTRY_FIXED_LEN - namelen,
                0,
            );
            value_offset += attr.value.len();
        }
        debug_assert_eq!(out.len(), data_start);
        for attr in &self.attrs {
            out.extend_from_slice(&attr.value);
        }
        out.extend_from_slice(fork);
        out
    }
}

/// The attribute entries behind an `ATTR` header whose Finder Info entry
/// ends at `finder_end`, checked the way XNU's `check_and_swap_attrhdr`
/// checks them: every structure inside the Finder Info entry, every value
/// inside the data area, and the values adding up to the area.
fn parse_attrs(image: &[u8], finder_end: usize) -> Result<Vec<Attr>, ParseError> {
    let total_size = be32(image, ATTR_HEADER_OFFSET + 8)?;
    let data_start = be32(image, ATTR_HEADER_OFFSET + 12)?;
    let data_length = be32(image, ATTR_HEADER_OFFSET + 16)?;
    let count = be16(image, ATTR_HEADER_OFFSET + 34)?;
    let data_end = data_start + data_length;
    if total_size > finder_end || data_start < ATTR_ENTRIES_OFFSET || data_end > total_size {
        return Err(ParseError::Invalid(
            "attribute area outside the Finder Info entry",
        ));
    }
    if count > MAX_ATTRS {
        return Err(ParseError::Invalid("too many attributes"));
    }
    let mut attrs = Vec::with_capacity(count);
    let mut at = ATTR_ENTRIES_OFFSET;
    let mut carried = 0;
    for _ in 0..count {
        let offset = be32(image, at)?;
        let length = be32(image, at + 4)?;
        let namelen = usize::from(field::<1>(image, at + 10)?[0]);
        let name_end = at + ATTR_ENTRY_FIXED_LEN + namelen;
        if namelen == 0 || name_end > data_start {
            return Err(ParseError::Invalid(
                "attribute entry runs into the data area",
            ));
        }
        let name = &image[at + ATTR_ENTRY_FIXED_LEN..name_end];
        if name[namelen - 1] != 0 || name[..namelen - 1].contains(&0) {
            return Err(ParseError::Invalid(
                "attribute name is not one NUL-terminated string",
            ));
        }
        if offset < data_start || offset + length > data_end {
            return Err(ParseError::Invalid("attribute value outside the data area"));
        }
        carried += length;
        attrs.push(Attr {
            name: name[..namelen - 1].to_vec(),
            value: image[offset..offset + length].to_vec(),
        });
        at += entry_len(namelen);
    }
    if carried != data_length {
        return Err(ParseError::Invalid(
            "attribute values do not fill the data area",
        ));
    }
    Ok(attrs)
}

