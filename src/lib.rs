#[macro_use]
extern crate lazy_static;

pub mod constants;
pub mod directory;
mod encryption;
pub mod header;

pub mod ftype;

pub use ftype::file_type;

pub mod error;
pub mod util;

pub type Result<T> = std::result::Result<T, Error>;

use crate::{
    directory::{DirectoryEntry, DirectoryEntryRaw, ObjectType},
    ftype::OleFileType,
    header::{parse_raw_header, OleHeader},
};
use derivative::Derivative;
use error::{Error, HeaderErrorType};
use std::io::Cursor;
use std::io::Read;

#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub struct OleFile {
    header: OleHeader,
    #[derivative(Debug = "ignore")]
    sectors: Vec<Vec<u8>>,
    #[derivative(Debug = "ignore")]
    sector_allocation_table: Vec<u32>,
    #[derivative(Debug = "ignore")]
    short_sector_allocation_table: Vec<u32>,
    #[derivative(Debug = "ignore")]
    pub directory_stream_data: Vec<u8>,
    pub directory_entries: Vec<DirectoryEntry>,
    #[derivative(Debug = "ignore")]
    mini_stream: Vec<[u8; 64]>,
    pub file_type: OleFileType,
    pub encrypted: bool,
}

impl OleFile {
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let f = Cursor::new(bytes);
        Self::parse(f)
    }

    pub fn root(&self) -> &DirectoryEntry {
        &self.directory_entries[0]
    }

    pub fn list_streams(&self) -> Vec<String> {
        self.list_object(ObjectType::Stream)
    }

    pub fn list_storage(&self) -> Vec<String> {
        self.list_object(ObjectType::Storage)
    }

    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    pub fn is_excel(&self) -> bool {
        return match self.file_type {
            OleFileType::Excel5 | OleFileType::Excel97 => true,
            _ => false,
        };
    }

    pub fn open_stream(&self, stream_path: &[&str]) -> Result<Vec<u8>> {
        if let Some(directory_entry) = self.find_stream(stream_path, None) {
            if directory_entry.object_type == ObjectType::Stream {
                let mut data = vec![];
                let mut collected_bytes = 0;
                // the unwrap is safe because the location is guaranteed to exist for this object type
                let mut next_sector = directory_entry.starting_sector_location.unwrap();

                if directory_entry.stream_size < self.header.standard_stream_min_size as u64 {
                    // it's in the mini-FAT
                    loop {
                        if next_sector == constants::CHAIN_END {
                            break;
                        } else {
                            let mut sector_data: Vec<u8> = vec![];
                            for byte in self.mini_stream[next_sector as usize] {
                                sector_data.push(byte);
                                collected_bytes += 1;
                                if collected_bytes == directory_entry.stream_size {
                                    break;
                                }
                            }
                            data.extend(sector_data)
                        }
                        next_sector = self.short_sector_allocation_table[next_sector as usize];
                    }
                } else {
                    // it's in the FAT
                    loop {
                        if next_sector == constants::CHAIN_END {
                            break;
                        } else {
                            let mut sector_data: Vec<u8> = vec![];
                            for byte in &self.sectors[next_sector as usize] {
                                sector_data.push(*byte);
                                collected_bytes += 1;
                                if collected_bytes == directory_entry.stream_size {
                                    break;
                                }
                            }
                            data.extend(sector_data)
                        }
                        next_sector = self.sector_allocation_table[next_sector as usize];
                    }
                }
                // println!("data.len(): {}", data.len());
                return Ok(data);
            }
        }

        Err(Error::OleDirectoryEntryNotFound)
    }

    fn list_object(&self, object_type: ObjectType) -> Vec<String> {
        self.directory_entries
            .iter()
            .filter_map(|entry| {
                if entry.object_type == object_type {
                    Some(entry.name.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    fn find_stream(
        &self,
        stream_path: &[&str],
        parent: Option<&DirectoryEntry>,
    ) -> Option<&DirectoryEntry> {
        let first_entry = stream_path[0];
        let remainder = &stream_path[1..];
        let remaining_len = remainder.len();

        match parent {
            Some(parent) => {
                // println!("recursive_parent_entry: {:?}", parent);
                // this is a recursive case
                let mut entries_to_search = vec![];
                if let Some(child_id) = parent.child_id {
                    let child = self.directory_entries.get(child_id as usize).unwrap();
                    entries_to_search.push((child, true));
                }
                if let Some(left_sibling_id) = parent.left_sibling_id {
                    entries_to_search.push((
                        self.directory_entries
                            .get(left_sibling_id as usize)
                            .unwrap(),
                        false,
                    ));
                }
                if let Some(right_sibling_id) = parent.right_sibling_id {
                    entries_to_search.push((
                        self.directory_entries
                            .get(right_sibling_id as usize)
                            .unwrap(),
                        false,
                    ));
                }
                for (entry, is_child) in entries_to_search {
                    if entry.name == first_entry {
                        return if remaining_len == 0 {
                            // println!("found_entry: {:?}", entry);
                            Some(entry)
                        } else if is_child {
                            self.find_stream(remainder, Some(entry))
                        } else {
                            self.find_stream(stream_path, Some(entry))
                        };
                    } else if let Some(found_entry) = self.find_stream(stream_path, Some(entry)) {
                        return Some(found_entry);
                    }
                }
                None
            }
            None => {
                //this is the root case
                if stream_path.is_empty() {
                    return None;
                }
                if let Some(found_entry) = self
                    .directory_entries
                    .iter()
                    .find(|entry| entry.name == first_entry)
                {
                    //handle this
                    if remaining_len == 0 {
                        // println!("found_entry: {:?}", found_entry);
                        Some(found_entry)
                    } else {
                        self.find_stream(remainder, Some(found_entry))
                    }
                } else {
                    None
                }
            }
        }
    }

    fn parse<R>(mut read: Cursor<R>) -> Result<Self>
    where
        R: AsRef<[u8]>,
    {
        // read the header
        let raw_file_header = parse_raw_header(&mut read)?;
        let file_header = OleHeader::from_raw(raw_file_header);
        let sector_size = file_header.sector_size as usize;

        //we have to read the remainder of the header if the sector size isn't what we tried to read
        if sector_size > constants::HEADER_LENGTH {
            let should_read_size = sector_size - constants::HEADER_LENGTH;
            let mut should_read = vec![0u8; should_read_size];
            let did_read_size = read.read(&mut should_read)?;
            if did_read_size != should_read_size {
                return Err(Error::OleInvalidHeader(HeaderErrorType::NotEnoughBytes(
                    should_read_size,
                    did_read_size,
                )));
            } else if should_read != vec![0u8; should_read_size] {
                return Err(Error::OleInvalidHeader(HeaderErrorType::Parsing(
                    "all bytes must be zero for larger header sizes",
                    "n/a".to_string(),
                )));
            }
        }

        let mut sectors = vec![];
        loop {
            let mut buf = vec![0u8; sector_size];
            match read.read(&mut buf) {
                Ok(actually_read_size) if actually_read_size == sector_size => {
                    sectors.push((&buf[0..actually_read_size]).to_vec());
                }
                Ok(wrong_size) if wrong_size != 0 => {
                    // TODO: we might have to handle the case where the
                    //      last sector isn't actually complete. Not sure yet.
                    //      the spec says the entire file has to be present here,
                    //      with equal sectors, so I'm doing it this way.
                    return Err(Error::OleUnexpectedEof(format!(
                        "short read when parsing sector number: {}",
                        sectors.len()
                    )));
                }
                Ok(_empty) => {
                    break;
                }
                Err(error) => {
                    return Err(Error::StdIo(error));
                }
            }
        }

        let mut self_to_init = OleFile {
            header: file_header,
            sectors,
            sector_allocation_table: vec![],
            short_sector_allocation_table: vec![],
            directory_stream_data: vec![],
            directory_entries: vec![],
            mini_stream: vec![],
            file_type: OleFileType::Generic,
            encrypted: false,
        };

        self_to_init.initialize_sector_allocation_table()?;
        self_to_init.initialize_short_sector_allocation_table()?;
        self_to_init.initialize_directory_stream()?;
        self_to_init.initialize_mini_stream()?;
        self_to_init.file_type = ftype::file_type(self_to_init.root());
        self_to_init.encrypted = encryption::is_encrypted(&self_to_init);
        Ok(self_to_init)
    }

    fn initialize_sector_allocation_table(&mut self) -> Result<()> {
        for sector_index in self.header.sector_allocation_table_head.iter() {
            // println!("sector_index: {:#x?}", *sector_index);
            if *sector_index == constants::UNALLOCATED_SECTOR
                || *sector_index == constants::CHAIN_END
            {
                break;
            }
            let sector = self.sectors[*sector_index as usize]
                .chunks_exact(4)
                .map(|quad| u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]));
            self.sector_allocation_table.extend(sector);
        }

        if self.header.master_sector_allocation_table_len > 0 {
            return Err(Error::CurrentlyUnimplemented(
                "MSAT/DI-FAT unsupported todo impl me".to_string(),
            ));
        }

        Ok(())
    }

    fn initialize_short_sector_allocation_table(&mut self) -> Result<()> {
        if self.header.short_sector_allocation_table_len == 0
            || self.header.short_sector_allocation_table_first_sector == constants::CHAIN_END
        {
            return Ok(()); // no mini stream here
        }

        let mut next_index = self.header.short_sector_allocation_table_first_sector;
        let mut short_sector_allocation_table_raw_data: Vec<u8> = vec![];
        loop {
            if next_index == constants::CHAIN_END {
                break;
            } else {
                short_sector_allocation_table_raw_data
                    .extend(self.sectors[next_index as usize].iter());
            }
            next_index = self.sector_allocation_table[next_index as usize];
        }

        self.short_sector_allocation_table.extend(
            short_sector_allocation_table_raw_data
                .chunks_exact(4)
                .map(|quad| u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]])),
        );

        Ok(())
    }

    fn initialize_directory_stream(&mut self) -> Result<()> {
        let mut next_directory_index = self.header.sector_allocation_table_first_sector;
        self.directory_stream_data
            .extend(self.sectors[next_directory_index as usize].iter());

        loop {
            next_directory_index = self.sector_allocation_table[next_directory_index as usize];
            if next_directory_index == constants::CHAIN_END {
                break;
            } else {
                self.directory_stream_data
                    .extend(self.sectors[next_directory_index as usize].iter());
            }
        }

        self.initialize_directory_entries()?;

        Ok(())
    }

    fn initialize_directory_entries(&mut self) -> Result<()> {
        if self.directory_stream_data.len() % constants::SIZE_OF_DIRECTORY_ENTRY != 0 {
            return Err(Error::OleInvalidDirectoryEntry(
                "directory_stream_size",
                format!(
                    "size of directory stream data is not correct? {}",
                    self.directory_stream_data.len()
                ),
            ));
        }

        self.directory_entries = Vec::with_capacity(
            self.directory_stream_data.len() / constants::SIZE_OF_DIRECTORY_ENTRY,
        );
        for (index, unparsed_entry) in self
            .directory_stream_data
            .chunks(constants::SIZE_OF_DIRECTORY_ENTRY)
            .enumerate()
        {
            // println!("unparsed_entry: {}", unparsed_entry.len());
            let raw_directory_entry = DirectoryEntryRaw::parse(unparsed_entry)?;
            match DirectoryEntry::from_raw(&self.header, raw_directory_entry, index) {
                Ok(directory_entry) => self.directory_entries.push(directory_entry),
                Err(Error::OleUnknownOrUnallocatedDirectoryEntry) => continue,
                Err(anything_else) => return Err(anything_else),
            }
        }

        Ok(())
    }

    fn initialize_mini_stream(&mut self) -> Result<()> {
        let (mut next_sector, mini_stream_size) = {
            let root_entry = &self.directory_entries[0];
            match root_entry.starting_sector_location {
                None => return Ok(()), //no mini-stream here
                Some(starting_sector_location) => {
                    (starting_sector_location, root_entry.stream_size)
                }
            }
        };

        let mut raw_mini_stream_data: Vec<u8> = vec![];
        loop {
            if next_sector == constants::CHAIN_END {
                break;
            } else {
                raw_mini_stream_data.extend(self.sectors[next_sector as usize].iter());
            }
            next_sector = self.sector_allocation_table[next_sector as usize];
        }
        raw_mini_stream_data.truncate(mini_stream_size as usize);
        raw_mini_stream_data.chunks_exact(64).for_each(|chunk| {
            self.mini_stream.push(<[u8; 64]>::try_from(chunk).unwrap());
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Read;

    use super::*;

    #[test]
    pub fn test_word_encryption_detection_on() {
        // Read file into vector
        let mut buf = vec![];
        let mut file = File::open("./data/encryption/encrypted/rc4cryptoapi_password.doc").unwrap();
        file.read_to_end(&mut buf).unwrap();

        let ole_file = OleFile::from_bytes(buf).expect("file not found");
        assert!(ole_file.is_encrypted());
    }

    #[test]
    pub fn test_word_encryption_detection_off() {
        let mut buf = vec![];
        let mut file = File::open("./data/encryption/plaintext/plain.doc").unwrap();

        file.read_to_end(&mut buf).unwrap();

        let ole_file = OleFile::from_bytes(buf).expect("file not found");

        assert!(!ole_file.is_encrypted());
    }

    #[test]
    pub fn test_excel_encryption_detection_on() {
        let mut buf = vec![];
        let mut file = File::open("./data/encryption/encrypted/rc4cryptoapi_password.xls").unwrap();

        file.read_to_end(&mut buf).unwrap();

        let ole_file = OleFile::from_bytes(buf).expect("file not found");
        assert!(ole_file.is_encrypted());
    }

    #[test]
    pub fn test_excel_encryption_detection_off() {
        let mut buf = vec![];
        let mut file = File::open("./data/encryption/plaintext/plain.xls").unwrap();

        file.read_to_end(&mut buf).unwrap();

        let ole_file = OleFile::from_bytes(buf).expect("file not found");

        assert!(!ole_file.is_encrypted());
    }
}
