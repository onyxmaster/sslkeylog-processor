use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
    io::BufRead,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{bail, Context, Result};
use mongodb::bson;
use regex::Regex;
use time::{format_description::FormatItem, macros::format_description, Duration};

use crate::{
    data_model::{EnrichedRecord, Record},
    errors,
    geolocator::Geolocator,
    logging,
    storage::Store,
};

pub(crate) struct Processor<'a> {
    filter: Option<&'a Regex>,
    term_token: &'a Arc<AtomicBool>,
    store: &'a mut Store<'a>,
    geolocator: Option<&'a Geolocator>,
}

impl<'a> Processor<'a> {
    pub fn new(
        filter: Option<&'a Regex>,
        term_token: &'a Arc<AtomicBool>,
        store: &'a mut Store<'a>,
        geolocator: Option<&'a Geolocator>,
    ) -> Self {
        Self {
            filter,
            term_token,
            store,
            geolocator,
        }
    }

    pub fn process<Paths>(&mut self, paths: Paths) -> Result<()>
    where
        Paths: IntoIterator,
        Paths::Item: AsRef<str>,
    {
        let mut failure = None;
        let mut batch_map = HashMap::<String, Vec<bson::Document>>::new();
        let mut next_collection_names = HashSet::new();
        for path in paths {
            if self.term_token.load(Ordering::Relaxed) {
                bail!(errors::TerminatedError::new("path iteration"));
            }

            if let Err(f) = self.process_file(&PathBuf::from(path.as_ref()), &mut batch_map, &mut next_collection_names) {
                logging::print(&f);
                if failure.is_none() {
                    failure = Some(f);
                }
            }
        }

        for (collection_name, batch) in batch_map {
            if self.term_token.load(Ordering::Relaxed) {
                bail!(errors::TerminatedError::new("flushing"));
            }

            let count = batch.len();
            println!("flushing {} to {}", count, collection_name);
            self.store
                .write(&collection_name, batch)
                .with_context(|| format!("Failed to flush {} to {}", count, collection_name))?;
        }

        for collection_name in next_collection_names {
            if self.term_token.load(Ordering::Relaxed) {
                bail!(errors::TerminatedError::new("ensuring"));
            }

            println!("ensuring {}", collection_name);
            self.store.ensure_collection(&collection_name);
        }

        failure.map(|f| bail!(f.context("Failed to process files"))).unwrap_or(Ok(()))
    }

    fn process_file(
        &mut self,
        path: &std::path::Path,
        batch_map: &mut HashMap<String, Vec<bson::Document>>,
        next_collection_names: &mut HashSet<String>,
    ) -> Result<()> {
        let file_name = &path.display();

        println!("{}: open", file_name);
        let file = std::fs::File::open(path).with_context(|| format!("Failed to open file {}", file_name))?;
        let lines = std::io::BufReader::new(file).lines();
        self.process_lines(lines, file_name, batch_map, next_collection_names)?;
        println!("{}: done", file_name);
        Ok(())
    }

    fn process_lines<Lines, Line, Error>(
        &mut self,
        lines: Lines,
        file_name: &impl std::fmt::Display,
        batch_map: &mut HashMap<String, Vec<bson::Document>>,
        next_collection_names: &mut HashSet<String>,
    ) -> Result<()>
    where
        Lines: IntoIterator<Item = Result<Line, Error>>,
        Line: AsRef<str>,
        Error: std::error::Error + Send + Sync + 'static,
    {
        let mut line_num = 0u64;
        let mut failure = None;
        for line in lines {
            line_num += 1;
            let location = FileLocation { file_name, line_num };

            if self.term_token.load(Ordering::Relaxed) {
                bail!(errors::TerminatedError::new(format!("processing {}", location)));
            }

            match self.process_line(&location, line, batch_map) {
                Ok(Some(n)) => {
                    next_collection_names.insert(n);
                }
                Ok(_) => {}
                Err(f) => {
                    logging::print(&f);
                    if failure.is_none() {
                        failure = Some(f);
                    }
                }
            }
        }

        failure
            .map(|f| bail!(f.context(format!("Failed to process lines of {}", file_name))))
            .unwrap_or(Ok(()))
    }

    fn process_line<Line: AsRef<str>, Error: std::error::Error + Send + Sync + 'static>(
        &mut self,
        location: &FileLocation,
        line: Result<Line, Error>,
        batch_map: &mut HashMap<String, Vec<bson::Document>>,
    ) -> Result<Option<String>> {
        let line = line.with_context(|| format!("Failed to read line at {}", location))?;
        let record = Record::try_from(line.as_ref()).with_context(|| format!("Failed to parse at {}", location))?;
        if self
            .filter
            .map(|f| !f.is_match(&format!("{}:{}", record.sni, record.server_port)))
            .unwrap_or(false)
        {
            return Ok(None);
        }

        let geolocation = self
            .geolocator
            .map(|g| {
                g.locate(record.client_ip)
                    .with_context(|| format!("Failed to locate client {} at {}", record.client_ip, location))
            })
            .transpose()?
            .flatten();

        let document = match geolocation {
            Some(geoname_id) => bson::Document::from(&EnrichedRecord {
                record: &record,
                geoname_id,
            }),
            None => bson::Document::from(&record),
        };

        const SUFFIX_FORMAT: &[FormatItem] = format_description!("[year][month][day]");
        let collection_name = format!(
            "{}@{}:{}_{}",
            record.sni,
            record.server_ip,
            record.server_port,
            record.timestamp.format(SUFFIX_FORMAT).unwrap()
        );
        self.write_document(&collection_name, document, location, batch_map)?;

        let next_collection_name = format!(
            "{}@{}:{}_{}",
            record.sni,
            record.server_ip,
            record.server_port,
            (record.timestamp + Duration::DAY).format(SUFFIX_FORMAT).unwrap()
        );
        Ok(Some(next_collection_name))
    }

    fn write_document(
        &mut self,
        collection_name: &str,
        document: bson::Document,
        location: &FileLocation,
        batch_map: &mut HashMap<String, Vec<bson::Document>>,
    ) -> Result<()> {
        let batch = batch_map.entry(collection_name.to_string()).or_insert_with(Vec::new);
        batch.push(document);
        let len = batch.len();
        const BATCH_SIZE: usize = 173;
        if len >= BATCH_SIZE {
            println!("{}: writing {} to {}", location.file_name, len, collection_name);
            let batch = batch_map.remove(collection_name).unwrap();
            self.store
                .write(collection_name, batch)
                .with_context(|| format!("Failed to write to {} for {}", collection_name, location.file_name))?;
        };
        Ok(())
    }
}

struct FileLocation<'a> {
    pub file_name: &'a dyn std::fmt::Display,
    pub line_num: u64,
}

impl std::fmt::Display for FileLocation<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}:{}", self.file_name, self.line_num))
    }
}
