//! EPUB (ZIP) read/write + OPF manifest parsing.

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub struct Entry {
    pub name: String,
    pub data: Vec<u8>,
    pub method: CompressionMethod,
    pub is_dir: bool,
}

/// Bound both advertised sizes and actual decompressed bytes. Upload limits
/// apply to compressed bytes and cannot protect the parser on their own.
#[derive(Clone, Copy)]
pub(crate) struct ArchiveLimits {
    pub entries: usize,
    pub entry_bytes: u64,
    pub total_bytes: u64,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            entries: 10_000,
            entry_bytes: 128 * 1024 * 1024,
            total_bytes: 512 * 1024 * 1024,
        }
    }
}

pub(crate) fn read_entries<R: Read + Seek>(
    za: &mut ZipArchive<R>,
    limits: ArchiveLimits,
) -> Result<Vec<Entry>> {
    anyhow::ensure!(
        za.len() <= limits.entries,
        "archive exceeds entry count limit ({})",
        limits.entries
    );
    let mut advertised = 0u64;
    for index in 0..za.len() {
        let entry = za.by_index(index)?;
        anyhow::ensure!(
            entry.size() <= limits.entry_bytes,
            "archive entry exceeds size limit: {}",
            entry.name()
        );
        advertised = advertised
            .checked_add(entry.size())
            .context("archive size overflow")?;
        anyhow::ensure!(
            advertised <= limits.total_bytes,
            "archive exceeds total decompressed size limit"
        );
    }
    let mut remaining = limits.total_bytes;
    let mut entries = Vec::with_capacity(za.len());
    for index in 0..za.len() {
        let mut entry = za.by_index(index)?;
        let name = entry.name().to_owned();
        let is_dir = entry.is_dir();
        let method = entry.compression();
        let data = read_bounded(&mut entry, limits.entry_bytes.min(remaining))
            .with_context(|| format!("decompress archive entry {name}"))?;
        remaining -= data.len() as u64;
        entries.push(Entry {
            name,
            data,
            method,
            is_dir,
        });
    }
    Ok(entries)
}

fn read_bounded(reader: &mut impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut data)?;
    anyhow::ensure!(
        data.len() as u64 <= limit,
        "archive decompressed byte limit exceeded"
    );
    Ok(data)
}

pub struct Epub {
    pub entries: Vec<Entry>,
}

impl Epub {
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut za = ZipArchive::new(file).context("read zip")?;
        let entries = read_entries(&mut za, ArchiveLimits::default())?;
        Ok(Epub { entries })
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.name == name)
    }

    pub fn read_text(&self, name: &str) -> Result<String> {
        let idx = self
            .find(name)
            .ok_or_else(|| anyhow!("entry not found: {}", name))?;
        Ok(String::from_utf8_lossy(&self.entries[idx].data).into_owned())
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
        let mut zw = ZipWriter::new(file);

        // EPUB validity: mimetype must be the first entry, uncompressed, no extra.
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let deflate = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        if let Some(m) = self.entries.iter().find(|e| e.name == "mimetype") {
            zw.start_file("mimetype", stored)?;
            zw.write_all(&m.data)?;
        }

        for e in self.entries.iter().filter(|e| e.name != "mimetype") {
            if e.is_dir {
                zw.add_directory(&e.name, deflate)?;
            } else {
                // Preserve each entry's original compression where possible.
                let opts = SimpleFileOptions::default().compression_method(e.method);
                zw.start_file(&e.name, opts)?;
                zw.write_all(&e.data)?;
            }
        }
        zw.finish().context("finish zip")?;
        Ok(())
    }

    /// Resolve the ordered list of spine content (xhtml) file paths within the
    /// archive, excluding the navigation document.
    pub fn content_files(&self) -> Result<Vec<String>> {
        let container = self
            .read_text("META-INF/container.xml")
            .context("EPUB missing META-INF/container.xml")?;
        let doc = roxmltree::Document::parse(&container)?;
        let opf_path = doc
            .descendants()
            .find(|n| n.has_tag_name("rootfile"))
            .and_then(|n| n.attribute("full-path"))
            .ok_or_else(|| anyhow!("container.xml has no rootfile/@full-path"))?
            .to_string();

        let opf_dir = parent_dir(&opf_path);
        let opf_xml = self
            .read_text(&opf_path)
            .with_context(|| format!("read OPF {}", opf_path))?;
        let opf = roxmltree::Document::parse(&opf_xml).context("parse OPF")?;

        // id -> (href, media-type, properties)
        let mut items: HashMap<String, (String, String, String)> = HashMap::new();
        for n in opf.descendants().filter(|n| n.has_tag_name("item")) {
            let id = match n.attribute("id") {
                Some(v) => v.to_string(),
                None => continue,
            };
            let href = n.attribute("href").unwrap_or("").to_string();
            let mt = n.attribute("media-type").unwrap_or("").to_string();
            let props = n.attribute("properties").unwrap_or("").to_string();
            items.insert(id, (href, mt, props));
        }

        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for n in opf.descendants().filter(|n| n.has_tag_name("itemref")) {
            let idref = match n.attribute("idref") {
                Some(v) => v,
                None => continue,
            };
            let (href, mt, props) = match items.get(idref) {
                Some(v) => v,
                None => continue,
            };
            if mt != "application/xhtml+xml" {
                continue;
            }
            // Skip the navigation document.
            if props.split_whitespace().any(|p| p == "nav") {
                continue;
            }
            let archive = join_path(&opf_dir, href);
            if seen.insert(archive.clone()) {
                result.push(archive);
            }
        }

        if result.is_empty() {
            return Err(anyhow!(
                "OPF spine yielded no xhtml content items; check {}",
                opf_path
            ));
        }
        Ok(result)
    }
}

fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

/// Join an OPF-relative href onto the OPF directory, normalising separators.
fn join_path(dir: &str, href: &str) -> String {
    // Decode the URL path, not a fragment or query component.
    let path = href.split(['#', '?']).next().unwrap_or(href);
    let href = percent_encoding::percent_decode_str(path)
        .decode_utf8_lossy()
        .replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for seg in dir.split('/').chain(href.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(seg),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spine_paths_decode_urls_after_removing_fragments() {
        assert_eq!(
            join_path("OEBPS", "chapter%201.xhtml#part"),
            "OEBPS/chapter 1.xhtml"
        );
        assert_eq!(join_path("", "%E4%B8%AD.xhtml"), "中.xhtml");
        assert_eq!(join_path("", "part%23one.xhtml"), "part#one.xhtml");
    }

    #[test]
    fn archive_limits_reject_count_entry_and_total_sizes() {
        use std::io::Cursor;
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for name in ["one", "two"] {
            writer
                .start_file(
                    name,
                    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(&[b'a'; 64]).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        for limits in [
            ArchiveLimits {
                entries: 1,
                entry_bytes: 128,
                total_bytes: 256,
            },
            ArchiveLimits {
                entries: 2,
                entry_bytes: 63,
                total_bytes: 256,
            },
            ArchiveLimits {
                entries: 2,
                entry_bytes: 64,
                total_bytes: 127,
            },
        ] {
            let mut zip = ZipArchive::new(Cursor::new(&bytes)).unwrap();
            assert!(read_entries(&mut zip, limits).is_err());
        }
        let mut zip = ZipArchive::new(Cursor::new(&bytes)).unwrap();
        assert_eq!(
            read_entries(
                &mut zip,
                ArchiveLimits {
                    entries: 2,
                    entry_bytes: 64,
                    total_bytes: 128
                }
            )
            .unwrap()
            .len(),
            2
        );
        // Enforce the actual stream size even when metadata understates it.
        assert!(read_bounded(&mut Cursor::new(vec![0; 65]), 64).is_err());
        assert_eq!(
            read_bounded(&mut Cursor::new(vec![0; 64]), 64)
                .unwrap()
                .len(),
            64
        );
    }

    #[test]
    fn parent_dir_cases() {
        assert_eq!(parent_dir("OEBPS/content.opf"), "OEBPS");
        assert_eq!(parent_dir("a/b/c.opf"), "a/b");
        assert_eq!(parent_dir("content.opf"), "");
        assert_eq!(parent_dir("/abs.opf"), "");
    }

    #[test]
    fn join_path_basic() {
        assert_eq!(join_path("OEBPS", "ch1.xhtml"), "OEBPS/ch1.xhtml");
        assert_eq!(join_path("", "ch1.xhtml"), "ch1.xhtml");
    }

    #[test]
    fn join_path_parent_reference() {
        assert_eq!(join_path("OEBPS", "../ch1.xhtml"), "ch1.xhtml");
        assert_eq!(join_path("a/b", "../c.xhtml"), "a/c.xhtml");
    }

    #[test]
    fn join_path_normalises_separators_and_dot() {
        assert_eq!(join_path("OEBPS", "sub\\ch1.xhtml"), "OEBPS/sub/ch1.xhtml");
        assert_eq!(join_path("a/b", "c/./d.xhtml"), "a/b/c/d.xhtml");
    }
}
