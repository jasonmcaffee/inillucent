//! Loading the generated corpus into PostgreSQL for the pgvector baseline.
//!
//! Invariant: **the schema is the one the original stack used.** The baseline
//! SQL in `engine.rs` runs unchanged against it - the same two tables, the same
//! columns it joins and filters on, the same HNSW index with the same
//! parameters and the same English full text index - so a score difference
//! between the two engines cannot come from one of them having been given a
//! different schema to work with.

use std::collections::HashMap;

use anyhow::{Context, Result};

use super::SynthChunk;

/// The schema the pgvector baseline queries. It is the schema the original stack
/// used, reproduced here so the baseline SQL in `engine.rs` runs unchanged: the
/// same two tables, the same columns it joins and filters on, the same HNSW index
/// with the same parameters, and the same English full text index.
const SCHEMA: &str = "
    DROP TABLE IF EXISTS chunks;
    DROP TABLE IF EXISTS documents;

    CREATE TABLE documents (
      id           bigint PRIMARY KEY,
      source       text NOT NULL,
      source_id    text NOT NULL,
      space_key    text,
      title        text NOT NULL,
      url          text NOT NULL,
      author       text,
      author_id    text,
      created_at   timestamptz,
      updated_at   timestamptz,
      labels       text[] NOT NULL DEFAULT '{}',
      content_hash text NOT NULL,
      deleted_at   timestamptz,
      synced_at    timestamptz NOT NULL DEFAULT now(),
      UNIQUE (source, source_id)
    );

    CREATE TABLE chunks (
      id              bigserial PRIMARY KEY,
      document_id     bigint NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
      chunk_index     integer NOT NULL,
      heading_path    text[] NOT NULL DEFAULT '{}',
      content         text NOT NULL,
      token_count     integer,
      embedding       vector(768),
      embedding_model text,
      UNIQUE (document_id, chunk_index)
    );
";

/// The indexes, created after the rows are inserted because building an HNSW index
/// once over a full table is far quicker than maintaining it per insert.
const INDEXES: &[(&str, &str)] = &[
    ("documents_source_idx", "CREATE INDEX documents_source_idx ON documents (source)"),
    ("documents_deleted_at_idx", "CREATE INDEX documents_deleted_at_idx ON documents (deleted_at)"),
    ("documents_space_key_idx", "CREATE INDEX documents_space_key_idx ON documents (space_key)"),
    ("documents_author_id_idx", "CREATE INDEX documents_author_id_idx ON documents (author_id)"),
    ("documents_updated_at_idx", "CREATE INDEX documents_updated_at_idx ON documents (updated_at DESC)"),
    ("documents_labels_gin_idx", "CREATE INDEX documents_labels_gin_idx ON documents USING gin (labels)"),
    ("chunks_document_id_idx", "CREATE INDEX chunks_document_id_idx ON chunks (document_id)"),
    (
        "chunks_content_fts",
        "CREATE INDEX chunks_content_fts ON chunks USING gin (to_tsvector('english', content))",
    ),
    (
        "chunks_embedding_hnsw",
        "CREATE INDEX chunks_embedding_hnsw ON chunks USING hnsw (embedding vector_cosine_ops) WITH (m = 16, ef_construction = 64)",
    ),
];

/// Makes sure the connected database can store and index a vector, whichever way
/// pgvector was installed into it.
///
/// `CREATE EXTENSION vector` is the normal path and is tried first. It fails on a
/// cluster where pgvector was installed by running its SQL with absolute paths to
/// the shared library, which is what a machine does when the PostgreSQL install
/// directory is not writable — the types, operators and both access methods are
/// all there, but no `vector.control` is on the extension path, so the extension
/// does not exist by name. Refusing to run there would be refusing over a name.
/// So the failure is only fatal when the type really is absent.
/// @param client - a connection to the database being loaded
fn ensure_pgvector(client: &mut postgres::Client) -> Result<()> {
    if client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS vector")
        .is_ok()
    {
        return Ok(());
    }
    let row = client
        .query_one("SELECT to_regtype('vector') IS NOT NULL", &[])
        .context("checking whether the vector type exists")?;
    let present: bool = row.get(0);
    anyhow::ensure!(
        present,
        "this database has no pgvector: CREATE EXTENSION vector failed and there is no vector type. \
         Install the extension, or run pgvector's SQL with absolute paths to vector.dll/vector.so."
    );
    Ok(())
}

/// Insert the corpus and its vectors, then build the indexes.
///
/// The vectors written here are the same bytes the cache holds, so the two engines
/// are compared on identical input. Nothing is recomputed.
pub fn load_postgres(
    url: &str,
    chunks: &[SynthChunk],
    corpus: &crate::corpus::Corpus,
    build_indexes: bool,
) -> Result<()> {
    use pgvector::Vector;
    use postgres::{Client, NoTls};

    anyhow::ensure!(
        chunks.len() == corpus.chunks.len(),
        "the corpus file holds {} chunks and the cache holds {}; rebuild the cache",
        chunks.len(),
        corpus.chunks.len()
    );

    let mut client = Client::connect(url, NoTls).with_context(|| {
        format!("connecting to {url}. Create the database first: createdb inillucent_synth")
    })?;

    eprintln!("creating the schema");
    ensure_pgvector(&mut client)?;
    client
        .batch_execute(SCHEMA)
        .context("creating the schema")?;

    // One row per document, taken from the first chunk that mentions it.
    eprintln!("inserting documents");
    let mut seen: HashMap<i64, ()> = HashMap::new();
    let mut documents = 0usize;
    {
        let mut tx = client.transaction()?;
        let statement = tx.prepare(
            "INSERT INTO documents
               (id, source, source_id, space_key, title, url, author, author_id,
                created_at, updated_at, labels, content_hash, deleted_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
        )?;
        for c in chunks {
            if seen.insert(c.doc_id, ()).is_some() {
                continue;
            }
            let updated = c
                .updated_at
                .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s.max(0) as u64));
            // A soft deleted document carries a deletion time, which is what every
            // filter excludes on.
            let deleted_at = if c.deleted {
                updated.or(Some(std::time::SystemTime::now()))
            } else {
                None
            };
            tx.execute(
                &statement,
                &[
                    &c.doc_id,
                    &c.source,
                    &format!("{}-{}", c.source, c.doc_id),
                    &c.space_key,
                    &c.title,
                    &c.url,
                    &c.author,
                    &c.author_id,
                    &updated,
                    &updated,
                    &c.labels,
                    &format!("{:016x}", c.doc_id),
                    &deleted_at,
                ],
            )?;
            documents += 1;
            if documents.is_multiple_of(5_000) {
                eprintln!("  {documents} documents");
            }
        }
        tx.commit()?;
    }
    eprintln!("  {documents} documents inserted");

    eprintln!("inserting chunks and their vectors");
    {
        let mut tx = client.transaction()?;
        let statement = tx.prepare(
            "INSERT INTO chunks
               (document_id, chunk_index, heading_path, content, token_count, embedding, embedding_model)
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
        )?;
        let model = "nomic-embed-text-v1.5";
        for (i, (c, stored)) in chunks.iter().zip(&corpus.vectors).enumerate() {
            let vector = Vector::from(stored.clone());
            // A rough token count, which the original column also held; nothing
            // queries it, but leaving it null would misrepresent the schema.
            let tokens = (c.content.len() / 4) as i32;
            tx.execute(
                &statement,
                &[
                    &c.doc_id,
                    &(c.chunk_index as i32),
                    &c.heading_path,
                    &c.content,
                    &tokens,
                    &vector,
                    &model,
                ],
            )?;
            if (i + 1) % 20_000 == 0 {
                eprintln!("  {}/{} chunks", i + 1, chunks.len());
            }
        }
        tx.commit()?;
    }
    eprintln!("  {} chunks inserted", chunks.len());

    if build_indexes {
        for (name, sql) in INDEXES {
            let start = std::time::Instant::now();
            eprintln!("building {name}");
            client
                .batch_execute(sql)
                .with_context(|| format!("building {name}"))?;
            eprintln!("  {name} in {:.1}s", start.elapsed().as_secs_f64());
        }
        client.batch_execute("ANALYZE documents; ANALYZE chunks;")?;
    } else {
        eprintln!("skipping indexes as asked; the baseline needs them before grading");
    }

    Ok(())
}
