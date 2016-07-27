// Copyright 2015, 2016 Ethcore (UK) Ltd.
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

//! Disk-backed `HashDB` implementation.

use common::*;
use rlp::*;
use hashdb::*;
use memorydb::*;
use super::{DB_PREFIX_LEN, LATEST_ERA_KEY};
use super::traits::JournalDB;
use kvdb::{Database, DBTransaction};
#[cfg(test)]
use std::env;

/// Suffix appended to auxiliary keys to distinguish them from normal keys.
/// Would be nich to use rocksdb columns for this eventually.
const AUX_FLAG: u8 = 255;

/// Implementation of the `HashDB` trait for a disk-backed database with a memory overlay
/// and latent-removal semantics.
///
/// Like `OverlayDB`, there is a memory overlay; `commit()` must be called in order to
/// write operations out to disk. Unlike `OverlayDB`, `remove()` operations do not take effect
/// immediately. Rather some age (based on a linear but arbitrary metric) must pass before
/// the removals actually take effect.
pub struct ArchiveDB {
	overlay: MemoryDB,
	backing: Arc<Database>,
	latest_era: Option<u64>,
	column: Option<u32>,
}

impl ArchiveDB {
	/// Create a new instance from file
	pub fn new(backing: Arc<Database>, col: Option<u32>) -> ArchiveDB {
		let latest_era = backing.get(col, &LATEST_ERA_KEY).expect("Low-level database error.").map(|val| decode::<u64>(&val));
		ArchiveDB {
			overlay: MemoryDB::new(),
			backing: backing,
			latest_era: latest_era,
			column: col,
		}
	}

	/// Create a new instance with an anonymous temporary database.
	#[cfg(test)]
	fn new_temp() -> ArchiveDB {
		let mut dir = env::temp_dir();
		dir.push(H32::random().hex());
		let backing = Arc::new(Database::open(&config, path).unwrap());
		Self::new(backing, Some(0))
	}

	fn payload(&self, key: &H256) -> Option<Bytes> {
		self.backing.get(self.column, key).expect("Low-level database error. Some issue with your hard disk?").map(|v| v.to_vec())
	}
}

impl HashDB for ArchiveDB {
	fn keys(&self) -> HashMap<H256, i32> {
		let mut ret: HashMap<H256, i32> = HashMap::new();
		for (key, _) in self.backing.iter(self.column) {
			let h = H256::from_slice(key.deref());
			ret.insert(h, 1);
		}

		for (key, refs) in self.overlay.keys().into_iter() {
			let refs = *ret.get(&key).unwrap_or(&0) + refs;
			ret.insert(key, refs);
		}
		ret
	}

	fn get(&self, key: &H256) -> Option<&[u8]> {
		let k = self.overlay.raw(key);
		match k {
			Some(&(ref d, rc)) if rc > 0 => Some(d),
			_ => {
				if let Some(x) = self.payload(key) {
					Some(&self.overlay.denote(key, x).0)
				}
				else {
					None
				}
			}
		}
	}

	fn contains(&self, key: &H256) -> bool {
		self.get(key).is_some()
	}

	fn insert(&mut self, value: &[u8]) -> H256 {
		self.overlay.insert(value)
	}

	fn emplace(&mut self, key: H256, value: Bytes) {
		self.overlay.emplace(key, value);
	}

	fn remove(&mut self, key: &H256) {
		self.overlay.remove(key);
	}

	fn insert_aux(&mut self, hash: Vec<u8>, value: Vec<u8>) {
		self.overlay.insert_aux(hash, value);
	}

	fn get_aux(&self, hash: &[u8]) -> Option<Vec<u8>> {
		if let Some(res) = self.overlay.get_aux(hash) {
			return Some(res)
		}

		let mut db_hash = hash.to_vec();
		db_hash.push(AUX_FLAG);

		self.backing.get(self.column, &db_hash)
			.expect("Low-level database error. Some issue with your hard disk?")
			.map(|v| v.to_vec())
	}

	fn remove_aux(&mut self, hash: &[u8]) {
		self.overlay.remove_aux(hash);
	}
}

impl JournalDB for ArchiveDB {
	fn boxed_clone(&self) -> Box<JournalDB> {
		Box::new(ArchiveDB {
			overlay: self.overlay.clone(),
			backing: self.backing.clone(),
			latest_era: self.latest_era,
			column: self.column.clone(),
		})
	}

	fn mem_used(&self) -> usize {
		self.overlay.mem_used()
 	}

	fn is_empty(&self) -> bool {
		self.latest_era.is_none()
	}

	fn commit(&mut self, batch: &DBTransaction, now: u64, _id: &H256, _end: Option<(u64, H256)>) -> Result<u32, UtilError> {
		let mut inserts = 0usize;
		let mut deletes = 0usize;

		for i in self.overlay.drain().into_iter() {
			let (key, (value, rc)) = i;
			if rc > 0 {
				assert!(rc == 1);
				batch.put(self.column, &key, &value).expect("Low-level database error. Some issue with your hard disk?");
				inserts += 1;
			}
			if rc < 0 {
				assert!(rc == -1);
				deletes += 1;
			}
		}

		for (mut key, value) in self.overlay.drain_aux().into_iter() {
			key.push(AUX_FLAG);
			batch.put(self.column, &key, &value).expect("Low-level database error. Some issue with your hard disk?");
		}

		if self.latest_era.map_or(true, |e| now > e) {
			try!(batch.put(self.column, &LATEST_ERA_KEY, &encode(&now)));
			self.latest_era = Some(now);
		}
		Ok((inserts + deletes) as u32)
	}

	fn latest_era(&self) -> Option<u64> { self.latest_era }

	fn state(&self, id: &H256) -> Option<Bytes> {
		self.backing.get_by_prefix(self.column, &id[0..DB_PREFIX_LEN]).map(|b| b.to_vec())
	}

	fn is_pruned(&self) -> bool { false }

	fn backing(&self) -> &Arc<Database> {
		&self.backing
	}
}

#[cfg(test)]
mod tests {
	#![cfg_attr(feature="dev", allow(blacklisted_name))]
	#![cfg_attr(feature="dev", allow(similar_names))]

	use common::*;
	use super::*;
	use hashdb::*;
	use journaldb::traits::JournalDB;
	use kvdb::DatabaseConfig;

	#[test]
	fn insert_same_in_fork() {
		// history is 1
		let mut jdb = ArchiveDB::new_temp();

		let x = jdb.insert(b"X");
		jdb.commit(1, &b"1".sha3(), None).unwrap();
		jdb.commit(2, &b"2".sha3(), None).unwrap();
		jdb.commit(3, &b"1002a".sha3(), Some((1, b"1".sha3()))).unwrap();
		jdb.commit(4, &b"1003a".sha3(), Some((2, b"2".sha3()))).unwrap();

		jdb.remove(&x);
		jdb.commit(3, &b"1002b".sha3(), Some((1, b"1".sha3()))).unwrap();
		let x = jdb.insert(b"X");
		jdb.commit(4, &b"1003b".sha3(), Some((2, b"2".sha3()))).unwrap();

		jdb.commit(5, &b"1004a".sha3(), Some((3, b"1002a".sha3()))).unwrap();
		jdb.commit(6, &b"1005a".sha3(), Some((4, b"1003a".sha3()))).unwrap();

		assert!(jdb.contains(&x));
	}

	#[test]
	fn long_history() {
		// history is 3
		let mut jdb = ArchiveDB::new_temp();
		let h = jdb.insert(b"foo");
		jdb.commit(0, &b"0".sha3(), None).unwrap();
		assert!(jdb.contains(&h));
		jdb.remove(&h);
		jdb.commit(1, &b"1".sha3(), None).unwrap();
		assert!(jdb.contains(&h));
		jdb.commit(2, &b"2".sha3(), None).unwrap();
		assert!(jdb.contains(&h));
		jdb.commit(3, &b"3".sha3(), Some((0, b"0".sha3()))).unwrap();
		assert!(jdb.contains(&h));
		jdb.commit(4, &b"4".sha3(), Some((1, b"1".sha3()))).unwrap();
	}

	#[test]
	fn complex() {
		// history is 1
		let mut jdb = ArchiveDB::new_temp();

		let foo = jdb.insert(b"foo");
		let bar = jdb.insert(b"bar");
		jdb.commit(0, &b"0".sha3(), None).unwrap();
		assert!(jdb.contains(&foo));
		assert!(jdb.contains(&bar));

		jdb.remove(&foo);
		jdb.remove(&bar);
		let baz = jdb.insert(b"baz");
		jdb.commit(1, &b"1".sha3(), Some((0, b"0".sha3()))).unwrap();
		assert!(jdb.contains(&foo));
		assert!(jdb.contains(&bar));
		assert!(jdb.contains(&baz));

		let foo = jdb.insert(b"foo");
		jdb.remove(&baz);
		jdb.commit(2, &b"2".sha3(), Some((1, b"1".sha3()))).unwrap();
		assert!(jdb.contains(&foo));
		assert!(jdb.contains(&baz));

		jdb.remove(&foo);
		jdb.commit(3, &b"3".sha3(), Some((2, b"2".sha3()))).unwrap();
		assert!(jdb.contains(&foo));

		jdb.commit(4, &b"4".sha3(), Some((3, b"3".sha3()))).unwrap();
	}

	#[test]
	fn fork() {
		// history is 1
		let mut jdb = ArchiveDB::new_temp();

		let foo = jdb.insert(b"foo");
		let bar = jdb.insert(b"bar");
		jdb.commit(0, &b"0".sha3(), None).unwrap();
		assert!(jdb.contains(&foo));
		assert!(jdb.contains(&bar));

		jdb.remove(&foo);
		let baz = jdb.insert(b"baz");
		jdb.commit(1, &b"1a".sha3(), Some((0, b"0".sha3()))).unwrap();

		jdb.remove(&bar);
		jdb.commit(1, &b"1b".sha3(), Some((0, b"0".sha3()))).unwrap();

		assert!(jdb.contains(&foo));
		assert!(jdb.contains(&bar));
		assert!(jdb.contains(&baz));

		jdb.commit(2, &b"2b".sha3(), Some((1, b"1b".sha3()))).unwrap();
		assert!(jdb.contains(&foo));
	}

	#[test]
	fn overwrite() {
		// history is 1
		let mut jdb = ArchiveDB::new_temp();

		let foo = jdb.insert(b"foo");
		jdb.commit(0, &b"0".sha3(), None).unwrap();
		assert!(jdb.contains(&foo));

		jdb.remove(&foo);
		jdb.commit(1, &b"1".sha3(), Some((0, b"0".sha3()))).unwrap();
		jdb.insert(b"foo");
		assert!(jdb.contains(&foo));
		jdb.commit(2, &b"2".sha3(), Some((1, b"1".sha3()))).unwrap();
		assert!(jdb.contains(&foo));
		jdb.commit(3, &b"2".sha3(), Some((0, b"2".sha3()))).unwrap();
		assert!(jdb.contains(&foo));
	}

	#[test]
	fn fork_same_key() {
		// history is 1
		let mut jdb = ArchiveDB::new_temp();
		jdb.commit(0, &b"0".sha3(), None).unwrap();

		let foo = jdb.insert(b"foo");
		jdb.commit(1, &b"1a".sha3(), Some((0, b"0".sha3()))).unwrap();

		jdb.insert(b"foo");
		jdb.commit(1, &b"1b".sha3(), Some((0, b"0".sha3()))).unwrap();
		assert!(jdb.contains(&foo));

		jdb.commit(2, &b"2a".sha3(), Some((1, b"1a".sha3()))).unwrap();
		assert!(jdb.contains(&foo));
	}

	#[test]
	fn reopen() {
		let mut dir = ::std::env::temp_dir();
		dir.push(H32::random().hex());
		let bar = H256::random();

		let foo = {
			let mut jdb = ArchiveDB::new(dir.to_str().unwrap(), DatabaseConfig::default());
			// history is 1
			let foo = jdb.insert(b"foo");
			jdb.emplace(bar.clone(), b"bar".to_vec());
			jdb.commit(0, &b"0".sha3(), None).unwrap();
			foo
		};

		{
			let mut jdb = ArchiveDB::new(dir.to_str().unwrap(), DatabaseConfig::default());
			jdb.remove(&foo);
			jdb.commit(1, &b"1".sha3(), Some((0, b"0".sha3()))).unwrap();
		}

		{
			let mut jdb = ArchiveDB::new(dir.to_str().unwrap(), DatabaseConfig::default());
			assert!(jdb.contains(&foo));
			assert!(jdb.contains(&bar));
			jdb.commit(2, &b"2".sha3(), Some((1, b"1".sha3()))).unwrap();
		}
	}

	#[test]
	fn reopen_remove() {
		let mut dir = ::std::env::temp_dir();
		dir.push(H32::random().hex());

		let foo = {
			let mut jdb = ArchiveDB::new(dir.to_str().unwrap(), DatabaseConfig::default());
			// history is 1
			let foo = jdb.insert(b"foo");
			jdb.commit(0, &b"0".sha3(), None).unwrap();
			jdb.commit(1, &b"1".sha3(), Some((0, b"0".sha3()))).unwrap();

			// foo is ancient history.

			jdb.insert(b"foo");
			jdb.commit(2, &b"2".sha3(), Some((1, b"1".sha3()))).unwrap();
			foo
		};

		{
			let mut jdb = ArchiveDB::new(dir.to_str().unwrap(), DatabaseConfig::default());
			jdb.remove(&foo);
			jdb.commit(3, &b"3".sha3(), Some((2, b"2".sha3()))).unwrap();
			assert!(jdb.contains(&foo));
			jdb.remove(&foo);
			jdb.commit(4, &b"4".sha3(), Some((3, b"3".sha3()))).unwrap();
			jdb.commit(5, &b"5".sha3(), Some((4, b"4".sha3()))).unwrap();
		}
	}

	#[test]
	fn reopen_fork() {
		let mut dir = ::std::env::temp_dir();
		dir.push(H32::random().hex());
		let (foo, _, _) = {
			let mut jdb = ArchiveDB::new(dir.to_str().unwrap(), DatabaseConfig::default());
			// history is 1
			let foo = jdb.insert(b"foo");
			let bar = jdb.insert(b"bar");
			jdb.commit(0, &b"0".sha3(), None).unwrap();
			jdb.remove(&foo);
			let baz = jdb.insert(b"baz");
			jdb.commit(1, &b"1a".sha3(), Some((0, b"0".sha3()))).unwrap();

			jdb.remove(&bar);
			jdb.commit(1, &b"1b".sha3(), Some((0, b"0".sha3()))).unwrap();
			(foo, bar, baz)
		};

		{
			let mut jdb = ArchiveDB::new(dir.to_str().unwrap(), DatabaseConfig::default());
			jdb.commit(2, &b"2b".sha3(), Some((1, b"1b".sha3()))).unwrap();
			assert!(jdb.contains(&foo));
		}
	}

	#[test]
	fn returns_state() {
		let temp = ::devtools::RandomTempPath::new();

		let key = {
			let mut jdb = ArchiveDB::new(temp.as_str(), DatabaseConfig::default());
			let key = jdb.insert(b"foo");
			jdb.commit(0, &b"0".sha3(), None).unwrap();
			key
		};

		{
			let jdb = ArchiveDB::new(temp.as_str(), DatabaseConfig::default());
			let state = jdb.state(&key);
			assert!(state.is_some());
		}
	}
}
