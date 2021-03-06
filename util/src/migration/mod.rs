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

//! DB Migration module.
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ::kvdb::{CompactionProfile, Database, DatabaseConfig, DBTransaction};

/// Migration config.
#[derive(Clone)]
pub struct Config {
	/// Defines how many elements should be migrated at once.
	pub batch_size: usize,
}

impl Default for Config {
	fn default() -> Self {
		Config {
			batch_size: 1024,
		}
	}
}

/// A batch of key-value pairs to be written into the database.
pub struct Batch {
	inner: BTreeMap<Vec<u8>, Vec<u8>>,
	batch_size: usize,
}

impl Batch {
	/// Make a new batch with the given config.
	pub fn new(config: &Config) -> Self {
		Batch {
			inner: BTreeMap::new(),
			batch_size: config.batch_size,
		}
	}

	/// Insert a value into the batch, committing if necessary.
	pub fn insert(&mut self, key: Vec<u8>, value: Vec<u8>, dest: &mut Database) -> Result<(), Error> {
		self.inner.insert(key, value);
		if self.inner.len() == self.batch_size {
			try!(self.commit(dest));
		}
		Ok(())
	}

	/// Commit all the items in the batch to the given database.
	pub fn commit(&mut self, dest: &mut Database) -> Result<(), Error> {
		if self.inner.is_empty() { return Ok(()) }

		let transaction = DBTransaction::new();

		for keypair in &self.inner {
			try!(transaction.put(&keypair.0, &keypair.1).map_err(Error::Custom));
		}

		self.inner.clear();
		dest.write(transaction).map_err(Error::Custom)
	}
}

/// Migration error.
#[derive(Debug)]
pub enum Error {
	/// Error returned when it is impossible to add new migration rules.
	CannotAddMigration,
	/// Error returned when migration from specific version can not be performed.
	MigrationImpossible,
	/// Io Error.
	Io(::std::io::Error),
	/// Custom error.
	Custom(String),
}

impl From<::std::io::Error> for Error {
	fn from(e: ::std::io::Error) -> Self {
		Error::Io(e)
	}
}

/// A generalized migration from the given db to a destination db.
pub trait Migration: 'static {
	/// Version of the database after the migration.
	fn version(&self) -> u32;
	/// Migrate a source to a destination.
	fn migrate(&mut self, source: &Database, config: &Config, destination: &mut Database) -> Result<(), Error>;
}

/// A simple migration over key-value pairs.
pub trait SimpleMigration: 'static {
	/// Version of database after the migration.
	fn version(&self) -> u32;
	/// Should migrate existing object to new database.
	/// Returns `None` if the object does not exist in new version of database.
	fn simple_migrate(&mut self, key: Vec<u8>, value: Vec<u8>) -> Option<(Vec<u8>, Vec<u8>)>;
}

impl<T: SimpleMigration> Migration for T {
	fn version(&self) -> u32 { SimpleMigration::version(self) }

	fn migrate(&mut self, source: &Database, config: &Config, dest: &mut Database) -> Result<(), Error> {
		let mut batch = Batch::new(config);

		for (key, value) in source.iter() {
			if let Some((key, value)) = self.simple_migrate(key.to_vec(), value.to_vec()) {
				try!(batch.insert(key, value, dest));
			}
		}

		batch.commit(dest)
	}
}

/// Get the path where all databases reside.
fn database_path(path: &Path) -> PathBuf {
	let mut temp_path = path.to_owned();
	temp_path.pop();
	temp_path
}

enum TempIndex {
	One,
	Two,
}

impl TempIndex {
	fn swap(&mut self) {
		match *self {
			TempIndex::One => *self = TempIndex::Two,
			TempIndex::Two => *self = TempIndex::One,
		}
	}

	// given the path to the old database, get the path of this one.
	fn path(&self, db_root: &Path) -> PathBuf {
		let mut buf = db_root.to_owned();

		match *self {
			TempIndex::One => buf.push("temp_migration_1"),
			TempIndex::Two => buf.push("temp_migration_2"),
		};

		buf
	}
}

/// Manages database migration.
pub struct Manager {
	config: Config,
	migrations: Vec<Box<Migration>>,
}

impl Manager {
	/// Creates new migration manager with given configuration.
	pub fn new(config: Config) -> Self {
		Manager {
			config: config,
			migrations: vec![],
		}
	}

	/// Adds new migration rules.
	pub fn add_migration<T>(&mut self, migration: T) -> Result<(), Error> where T: Migration {
		let version_match = match self.migrations.last() {
			Some(last) => last.version() + 1 == migration.version(),
			None => true,
		};

		match version_match {
			true => Ok(self.migrations.push(Box::new(migration))),
			false => Err(Error::CannotAddMigration),
		}
	}

	/// Performs migration in order, starting with a source path, migrating between two temporary databases,
	/// and producing a path where the final migration lives.
	pub fn execute(&mut self, old_path: &Path, version: u32) -> Result<PathBuf, Error> {
		let config = self.config.clone();
		let migrations = try!(self.migrations_from(version).ok_or(Error::MigrationImpossible));
		let db_config = DatabaseConfig {
			max_open_files: 64,
			cache_size: None,
			compaction: CompactionProfile::default(),
		};

		let db_root = database_path(old_path);
		let mut temp_idx = TempIndex::One;
		let mut temp_path = temp_idx.path(&db_root);

		// start with the old db.
		let old_path_str = try!(old_path.to_str().ok_or(Error::MigrationImpossible));
		let mut cur_db = try!(Database::open(&db_config, old_path_str).map_err(Error::Custom));
		for migration in migrations {
			// open the target temporary database.
			temp_path = temp_idx.path(&db_root);
			let temp_path_str = try!(temp_path.to_str().ok_or(Error::MigrationImpossible));
			let mut new_db = try!(Database::open(&db_config, temp_path_str).map_err(Error::Custom));

			// perform the migration from cur_db to new_db.
			try!(migration.migrate(&cur_db, &config, &mut new_db));
			// next iteration, we will migrate from this db into the other temp.
			cur_db = new_db;
			temp_idx.swap();

			// remove the other temporary migration database.
			let _ = fs::remove_dir_all(temp_idx.path(&db_root));
		}
		Ok(temp_path)
	}

	/// Returns true if migration is needed.
	pub fn is_needed(&self, version: u32) -> bool {
		match self.migrations.last() {
			Some(last) => version < last.version(),
			None => false,
		}
	}

	fn migrations_from(&mut self, version: u32) -> Option<&mut [Box<Migration>]> {
		// index of the first required migration
		let position = self.migrations.iter().position(|m| m.version() == version + 1);
		position.map(move |p| &mut self.migrations[p..])
	}
}

