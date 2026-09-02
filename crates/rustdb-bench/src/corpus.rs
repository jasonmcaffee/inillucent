//! Reading a corpus out of PostgreSQL, and caching it locally.
//!
//! The corpus this engine is graded on is built by `synth.rs` and embedded once,
//! and both engines then read the identical vectors: `synth-embed` writes them to
//! the cache and `synth-load` writes the same bytes into PostgreSQL. That is what
//! makes a score difference attributable to indexing and ranking rather than to
//! the embedding model.
//!
//! This module is the other direction, for grading against a corpus that already
//! lives in a database with this schema. The pull costs minutes and produces
//! roughly 600 MB of vectors, so it is cached in a plain binary file that loads by
//! a single read.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use pgvector::Vector;
use postgres::{Client, NoTls};
use rustdb_core::store::ChunkInput;

/// Bumped whenever the cache layout changes, so a stale file is rejected rather
/// than misread.
const CACHE_MAGIC: &[u8; 8] = b"RDBCACH3";

pub struct Corpus {
    pub chunks: Vec<ChunkInput>,
    pub vectors: Vec<Vec<f32>>,
    pub dims: usize,
}

impl Corpus {
    pub fn len(&self) -> usize {
        self.chunks.len()
    }
}

/// The query that defines the corpus. It mirrors the joins and the soft delete
/// rule the baseline's search uses, so the two engines see the same rows.
const CORPUS_SQL: &str = "
    SELECT
      c.id::text            AS chunk_id,
      d.source              AS source,
      d.id::text            AS doc_id,
      c.chunk_index         AS chunk_index,
      c.heading_path        AS heading_path,
      c.content             AS content,
      d.title               AS title,
      d.url                 AS url,
      d.space_key           AS space_key,
      d.author              AS author,
      d.author_id           AS author_id,
      d.updated_at          AS updated_at,
      d.labels              AS labels,
      (d.deleted_at IS NOT NULL) AS deleted,
      c.embedding           AS embedding
    FROM chunks c
    JOIN documents d ON d.id = c.document_id
    WHERE c.embedding IS NOT NULL
    ORDER BY c.id
";

pub fn load_from_postgres(url: &str, limit: Option<usize>) -> Result<Corpus> {
    let mut client = Client::connect(url, NoTls).context("connecting to PostgreSQL")?;

    let sql = match limit {
        Some(n) => format!("{CORPUS_SQL} LIMIT {n}"),
        None => CORPUS_SQL.to_string(),
    };

    // A portal keeps the 186k row result set off the client heap all at once.
    let mut chunks = Vec::new();
    let mut vectors = Vec::new();
    let mut dims = 0usize;

    let mut transaction = client.transaction()?;
    let statement = transaction.prepare(&sql)?;
    let portal = transaction.bind(&statement, &[])?;

    loop {
        let rows = transaction.query_portal(&portal, 5_000)?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            let embedding: Vector = row.get("embedding");
            let v = embedding.to_vec();
            if dims == 0 {
                dims = v.len();
            }
            if v.len() != dims {
                anyhow::bail!("mixed vector widths in the corpus: {} and {}", dims, v.len());
            }

            let updated: Option<std::time::SystemTime> = row.get("updated_at");
            let updated_at = updated.map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            });

            chunks.push(ChunkInput {
                source: row.get("source"),
                external_doc_id: row.get("doc_id"),
                chunk_index: row.get::<_, i32>("chunk_index") as u32,
                heading_path: row.get::<_, Vec<String>>("heading_path"),
                content: row.get("content"),
                title: row.get("title"),
                url: row.get("url"),
                space_key: row.get("space_key"),
                author: row.get("author"),
                author_id: row.get("author_id"),
                updated_at,
                external_chunk_id: None,
                labels: row.get::<_, Vec<String>>("labels"),
                attributes: Vec::new(),
                flags: Vec::new(),
                deleted: row.get("deleted"),
            });
            vectors.push(v);
        }
        eprintln!("  loaded {} chunks", chunks.len());
    }

    Ok(Corpus { chunks, vectors, dims })
}

fn write_string(w: &mut impl Write, s: &str) -> Result<()> {
    let bytes = s.as_bytes();
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    Ok(())
}

fn read_string(r: &mut impl Read) -> Result<String> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8(buf)?)
}

fn write_opt_string(w: &mut impl Write, s: &Option<String>) -> Result<()> {
    match s {
        Some(v) => {
            w.write_all(&[1u8])?;
            write_string(w, v)
        }
        None => {
            w.write_all(&[0u8])?;
            Ok(())
        }
    }
}

fn read_opt_string(r: &mut impl Read) -> Result<Option<String>> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    if tag[0] == 0 {
        Ok(None)
    } else {
        Ok(Some(read_string(r)?))
    }
}

pub fn save_cache(corpus: &Corpus, path: &Path) -> Result<()> {
    let mut w = BufWriter::new(File::create(path)?);
    w.write_all(CACHE_MAGIC)?;
    w.write_all(&(corpus.dims as u32).to_le_bytes())?;
    w.write_all(&(corpus.len() as u32).to_le_bytes())?;

    for (c, v) in corpus.chunks.iter().zip(&corpus.vectors) {
        write_string(&mut w, &c.source)?;
        write_string(&mut w, &c.external_doc_id)?;
        w.write_all(&c.chunk_index.to_le_bytes())?;
        w.write_all(&(c.heading_path.len() as u32).to_le_bytes())?;
        for h in &c.heading_path {
            write_string(&mut w, h)?;
        }
        write_string(&mut w, &c.content)?;
        write_string(&mut w, &c.title)?;
        write_string(&mut w, &c.url)?;
        write_opt_string(&mut w, &c.space_key)?;
        write_opt_string(&mut w, &c.author)?;
        write_opt_string(&mut w, &c.author_id)?;
        w.write_all(&c.updated_at.unwrap_or(i64::MIN).to_le_bytes())?;
        w.write_all(&(c.labels.len() as u32).to_le_bytes())?;
        for l in &c.labels {
            write_string(&mut w, l)?;
        }
        w.write_all(&[c.deleted as u8])?;
        for x in v {
            w.write_all(&x.to_le_bytes())?;
        }
    }
    w.flush()?;
    Ok(())
}

pub fn load_cache(path: &Path) -> Result<Corpus> {
    let mut r = BufReader::new(File::open(path)?);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != CACHE_MAGIC {
        anyhow::bail!("cache format does not match; delete the file and reload");
    }
    let mut buf4 = [0u8; 4];
    r.read_exact(&mut buf4)?;
    let dims = u32::from_le_bytes(buf4) as usize;
    r.read_exact(&mut buf4)?;
    let n = u32::from_le_bytes(buf4) as usize;

    let mut chunks = Vec::with_capacity(n);
    let mut vectors = Vec::with_capacity(n);
    let mut buf8 = [0u8; 8];
    let mut tag = [0u8; 1];

    for _ in 0..n {
        let source = read_string(&mut r)?;
        let external_doc_id = read_string(&mut r)?;
        r.read_exact(&mut buf4)?;
        let chunk_index = u32::from_le_bytes(buf4);
        r.read_exact(&mut buf4)?;
        let n_headings = u32::from_le_bytes(buf4) as usize;
        let mut heading_path = Vec::with_capacity(n_headings);
        for _ in 0..n_headings {
            heading_path.push(read_string(&mut r)?);
        }
        let content = read_string(&mut r)?;
        let title = read_string(&mut r)?;
        let url = read_string(&mut r)?;
        let space_key = read_opt_string(&mut r)?;
        let author = read_opt_string(&mut r)?;
        let author_id = read_opt_string(&mut r)?;
        r.read_exact(&mut buf8)?;
        let raw_updated = i64::from_le_bytes(buf8);
        let updated_at = if raw_updated == i64::MIN { None } else { Some(raw_updated) };
        r.read_exact(&mut buf4)?;
        let n_labels = u32::from_le_bytes(buf4) as usize;
        let mut labels = Vec::with_capacity(n_labels);
        for _ in 0..n_labels {
            labels.push(read_string(&mut r)?);
        }
        r.read_exact(&mut tag)?;
        let deleted = tag[0] != 0;

        let mut v = vec![0f32; dims];
        for x in v.iter_mut() {
            r.read_exact(&mut buf4)?;
            *x = f32::from_le_bytes(buf4);
        }

        chunks.push(ChunkInput {
            source,
            external_doc_id,
            chunk_index,
            heading_path,
            content,
            title,
            url,
            space_key,
            author,
            author_id,
            updated_at,
            external_chunk_id: None,
            labels,
            attributes: Vec::new(),
            flags: Vec::new(),
            deleted,
        });
        vectors.push(v);
    }

    Ok(Corpus { chunks, vectors, dims })
}
