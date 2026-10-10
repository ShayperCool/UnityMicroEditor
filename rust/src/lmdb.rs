//! Offline Linux x64 LMDB v1 images. Native LMDB owns live transactions.
use crate::protocol::{put_u16, put_u32, put_u64, u32_at, u64_at};
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, HashSet};
pub const PAGE_SIZE: usize = 4096;
pub const MAX_FILE_BYTES: usize = 128 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}
pub type Tables = BTreeMap<String, Vec<Record>>;
fn u16_at(data: &[u8], f: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        data.get(f..f + 2)
            .context("field outside LMDB page")?
            .try_into()?,
    ))
}
pub struct Image<'a> {
    data: &'a [u8],
    last_page: u64,
    root: u64,
}
impl<'a> Image<'a> {
    pub fn open(data: &'a [u8]) -> Result<Self> {
        ensure!(
            data.len() >= 3 * PAGE_SIZE
                && data.len().is_multiple_of(PAGE_SIZE)
                && data.len() <= MAX_FILE_BYTES,
            "LMDB image size is outside probe bounds"
        );
        let mut chosen: Option<(&[u8], u64)> = None;
        for i in 0..2 {
            let page = &data[i * PAGE_SIZE..(i + 1) * PAGE_SIZE];
            if u64_at(page, 0)? != i as u64
                || u16_at(page, 10)? != 8
                || u32_at(page, 16)? != 0xbeefc0de
                || u32_at(page, 20)? != 1
                || u32_at(page, 40)? != PAGE_SIZE as u32
            {
                continue;
            }
            let tx = u64_at(page, 144)?;
            if chosen.is_none_or(|(_, current)| tx > current) {
                chosen = Some((page, tx));
            }
        }
        let (page, _) = chosen.context("no valid Linux x64 LMDB v1 metadata page")?;
        let last_page = u64_at(page, 136)?;
        ensure!(
            last_page < (data.len() / PAGE_SIZE) as u64,
            "LMDB metadata references missing pages"
        );
        Ok(Self {
            data,
            last_page,
            root: u64_at(page, 128)?,
        })
    }
    pub fn table(&self, name: &str) -> Result<Vec<Record>> {
        for descriptor in self.records(self.root)? {
            if descriptor.key == name.as_bytes() {
                ensure!(
                    descriptor.value.len() == 48,
                    "LMDB named descriptor is invalid"
                );
                return self.records(u64_at(&descriptor.value, 40)?);
            }
        }
        bail!("LMDB table {name:?} is missing")
    }
    pub fn tables(&self) -> Result<Tables> {
        let mut tables = Tables::new();
        for descriptor in self.records(self.root)? {
            ensure!(
                descriptor.value.len() == 48,
                "LMDB named descriptor is invalid"
            );
            tables.insert(
                String::from_utf8(descriptor.key)?,
                self.records(u64_at(&descriptor.value, 40)?)?,
            );
        }
        Ok(tables)
    }
    fn records(&self, root: u64) -> Result<Vec<Record>> {
        let mut output = Vec::new();
        if root != u64::MAX {
            self.visit(root, 0, &mut HashSet::new(), &mut output)?;
        }
        Ok(output)
    }
    fn visit(
        &self,
        number: u64,
        depth: usize,
        seen: &mut HashSet<u64>,
        output: &mut Vec<Record>,
    ) -> Result<()> {
        ensure!(
            depth <= 32 && number >= 2 && number <= self.last_page && seen.insert(number),
            "LMDB tree contains an invalid page or cycle"
        );
        let page = &self.data[number as usize * PAGE_SIZE..(number as usize + 1) * PAGE_SIZE];
        ensure!(
            u64_at(page, 0)? == number,
            "LMDB page number does not match its location"
        );
        let flags = u16_at(page, 10)?;
        ensure!(
            flags == 1 || flags == 2,
            "unsupported LMDB tree page flags {flags:#x}"
        );
        let (lower, upper) = (u16_at(page, 12)? as usize, u16_at(page, 14)? as usize);
        ensure!(
            lower >= 16 && lower <= upper && upper <= PAGE_SIZE && (lower - 16).is_multiple_of(2),
            "LMDB page bounds are invalid"
        );
        for index in 0..(lower - 16) / 2 {
            let offset = u16_at(page, 16 + 2 * index)? as usize;
            ensure!(
                offset >= upper && offset + 8 <= PAGE_SIZE,
                "LMDB node points outside page data"
            );
            let node = &page[offset..];
            let key_size = u16_at(node, 6)? as usize;
            ensure!(key_size <= node.len() - 8, "LMDB node key exceeds page");
            let key = &node[8..8 + key_size];
            let node_flags = u16_at(node, 4)?;
            if flags == 1 {
                self.visit(
                    u32_at(node, 0)? as u64 | (node_flags as u64) << 32,
                    depth + 1,
                    seen,
                    output,
                )?;
                continue;
            }
            ensure!(
                node_flags & 4 == 0,
                "duplicate-value LMDB records are not supported"
            );
            let size = u32_at(node, 0)? as usize;
            let value = if node_flags & 1 != 0 {
                let overflow = u64_at(node, 8 + key_size)?;
                ensure!(
                    overflow >= 2 && overflow <= self.last_page,
                    "LMDB overflow page is invalid"
                );
                let start = overflow as usize * PAGE_SIZE;
                let header = &self.data[start..start + 16];
                let pages = u32_at(header, 12)? as u64;
                ensure!(
                    u16_at(header, 10)? == 4
                        && pages > 0
                        && overflow + pages - 1 <= self.last_page
                        && size as u64 <= pages * PAGE_SIZE as u64 - 16,
                    "LMDB overflow record exceeds allocated pages"
                );
                &self.data[start + 16..start + 16 + size]
            } else {
                node.get(8 + key_size..8 + key_size + size)
                    .context("LMDB leaf value exceeds page")?
            };
            output.push(Record {
                key: key.to_vec(),
                value: value.to_vec(),
            });
        }
        Ok(())
    }
}
#[derive(Default)]
struct Tree {
    root: u64,
    branch_pages: u64,
    leaf_pages: u64,
    overflow_pages: u64,
    depth: u16,
    entries: u64,
}
impl Tree {
    fn descriptor(&self) -> Vec<u8> {
        let mut data = vec![0; 48];
        put_u16(&mut data, 6, self.depth);
        for (f, value) in [
            (8, self.branch_pages),
            (16, self.leaf_pages),
            (24, self.overflow_pages),
            (32, self.entries),
            (40, self.root),
        ] {
            put_u64(&mut data, f, value);
        }
        data
    }
}
#[derive(Clone)]
struct Child {
    page: u64,
    key: Vec<u8>,
}
struct Builder {
    pages: Vec<Vec<u8>>,
}
fn node(key: &[u8], value: &[u8], flags: u16, size: u32) -> Vec<u8> {
    let mut data = vec![0; (8 + key.len() + value.len() + 1) & !1];
    put_u32(&mut data, 0, size);
    put_u16(&mut data, 4, flags);
    put_u16(&mut data, 6, key.len() as u16);
    data[8..8 + key.len()].copy_from_slice(key);
    data[8 + key.len()..8 + key.len() + value.len()].copy_from_slice(value);
    data
}
fn pack(nodes: &[Vec<u8>], flags: u16) -> Result<Vec<u8>> {
    let mut page = vec![0; PAGE_SIZE];
    put_u16(&mut page, 10, flags);
    let (mut lower, mut upper) = (16, PAGE_SIZE);
    for node in nodes {
        upper = upper
            .checked_sub(node.len())
            .context("LMDB page nodes exceed its bounds")?;
        ensure!(upper >= lower + 2, "LMDB page nodes exceed its bounds");
        put_u16(&mut page, lower, upper as u16);
        lower += 2;
        page[upper..upper + node.len()].copy_from_slice(node);
    }
    put_u16(&mut page, 12, lower as u16);
    put_u16(&mut page, 14, upper as u16);
    Ok(page)
}
impl Builder {
    fn append(&mut self, mut page: Vec<u8>) -> Result<u64> {
        ensure!(
            page.len() == PAGE_SIZE && (self.pages.len() + 1) * PAGE_SIZE <= MAX_FILE_BYTES,
            "LMDB image exceeds bounded size"
        );
        let n = self.pages.len() as u64;
        put_u64(&mut page, 0, n);
        self.pages.push(page);
        Ok(n)
    }
    fn overflow(&mut self, value: &[u8]) -> Result<(u64, u64)> {
        let count = (16 + value.len()).div_ceil(PAGE_SIZE);
        ensure!(
            (self.pages.len() + count) * PAGE_SIZE <= MAX_FILE_BYTES,
            "LMDB overflow exceeds bounded size"
        );
        let number = self.pages.len() as u64;
        let mut data = vec![0; count * PAGE_SIZE];
        put_u64(&mut data, 0, number);
        put_u16(&mut data, 10, 4);
        put_u32(&mut data, 12, count as u32);
        data[16..16 + value.len()].copy_from_slice(value);
        self.pages.extend(
            data.as_chunks::<PAGE_SIZE>()
                .0
                .iter()
                .map(|page| page.to_vec()),
        );
        Ok((number, count as u64))
    }
    fn flush(
        &mut self,
        nodes: &mut Vec<Vec<u8>>,
        children: &mut Vec<Child>,
        first: &mut Option<Vec<u8>>,
    ) -> Result<()> {
        let page = self.append(pack(nodes, 2)?)?;
        children.push(Child {
            page,
            key: first.take().context("empty leaf page")?,
        });
        nodes.clear();
        Ok(())
    }
    fn tree(&mut self, records: &[Record], subdatabases: bool) -> Result<Tree> {
        let mut tree = Tree {
            root: u64::MAX,
            entries: records.len() as u64,
            ..Tree::default()
        };
        if records.is_empty() {
            return Ok(tree);
        }
        let mut sorted: Vec<_> = records.iter().collect();
        sorted.sort_by(|a, b| a.key.cmp(&b.key));
        let (mut nodes, mut children, mut used, mut first) = (Vec::new(), Vec::new(), 16, None);
        for (i, record) in sorted.iter().enumerate() {
            ensure!(
                !record.key.is_empty()
                    && record.key.len() <= 511
                    && record.value.len() <= MAX_FILE_BYTES
                    && (i == 0 || record.key != sorted[i - 1].key),
                "invalid or duplicate LMDB key"
            );
            let mut flags = if subdatabases { 2 } else { 0 };
            let mut pointer;
            let value =
                if (8 + record.key.len() + record.value.len() + 1) & !1 > (PAGE_SIZE - 16) / 2 {
                    ensure!(!subdatabases, "LMDB descriptor exceeds native node bounds");
                    let (page, count) = self.overflow(&record.value)?;
                    tree.overflow_pages += count;
                    pointer = [0; 8];
                    put_u64(&mut pointer, 0, page);
                    flags |= 1;
                    &pointer[..]
                } else {
                    &record.value[..]
                };
            let data = node(&record.key, value, flags, record.value.len() as u32);
            if used + data.len() + 2 > PAGE_SIZE && !nodes.is_empty() {
                self.flush(&mut nodes, &mut children, &mut first)?;
                tree.leaf_pages += 1;
                used = 16;
            }
            if first.is_none() {
                first = Some(record.key.clone());
            }
            used += data.len() + 2;
            nodes.push(data);
        }
        self.flush(&mut nodes, &mut children, &mut first)?;
        tree.leaf_pages += 1;
        tree.depth = 1;
        while children.len() > 1 {
            let (mut groups, mut start, mut size) = (Vec::new(), 0, 16);
            for (i, child) in children.iter().enumerate() {
                let mut bytes = if i == start {
                    8
                } else {
                    (8 + child.key.len() + 1) & !1
                };
                if size + bytes + 2 > PAGE_SIZE {
                    groups.push((start, i));
                    start = i;
                    size = 16;
                    bytes = 8;
                }
                size += bytes + 2;
            }
            groups.push((start, children.len()));
            if groups.len() > 1 && groups.last().unwrap().1 - groups.last().unwrap().0 == 1 {
                let end = groups.len() - 1;
                groups[end].0 -= 1;
                groups[end - 1].1 -= 1;
            }
            let mut next = Vec::new();
            for (a, b) in groups {
                let branch: Vec<_> = children[a..b]
                    .iter()
                    .enumerate()
                    .map(|(i, child)| {
                        node(
                            if i == 0 { &[] } else { &child.key },
                            &[],
                            (child.page >> 32) as u16,
                            child.page as u32,
                        )
                    })
                    .collect();
                let page = self.append(pack(&branch, 1)?)?;
                next.push(Child {
                    page,
                    key: children[a].key.clone(),
                });
                tree.branch_pages += 1;
            }
            children = next;
            tree.depth += 1;
        }
        tree.root = children[0].page;
        Ok(tree)
    }
}
pub fn build(tables: &Tables) -> Result<Vec<u8>> {
    ensure!(
        !tables.is_empty() && tables.len() <= 64,
        "LMDB table count is outside bounds"
    );
    let mut builder = Builder {
        pages: vec![vec![0; PAGE_SIZE], vec![0; PAGE_SIZE]],
    };
    let mut descriptors = Vec::new();
    for (name, records) in tables {
        ensure!(
            !name.is_empty() && !name.contains('\0'),
            "LMDB table name is invalid"
        );
        let tree = builder
            .tree(records, false)
            .with_context(|| format!("LMDB table {name}"))?;
        descriptors.push(Record {
            key: name.as_bytes().to_vec(),
            value: tree.descriptor(),
        });
    }
    let root = builder.tree(&descriptors, true)?;
    let last = builder.pages.len() as u64 - 1;
    for i in 0..2 {
        let meta = &mut builder.pages[i];
        put_u64(meta, 0, i as u64);
        put_u16(meta, 10, 8);
        put_u32(meta, 16, 0xbeefc0de);
        put_u32(meta, 20, 1);
        put_u64(meta, 32, MAX_FILE_BYTES as u64);
        let mut free = Tree {
            root: u64::MAX,
            ..Tree::default()
        }
        .descriptor();
        put_u32(&mut free, 0, PAGE_SIZE as u32);
        put_u16(&mut free, 4, 8);
        meta[40..88].copy_from_slice(&free);
        meta[88..136].copy_from_slice(&root.descriptor());
        put_u64(meta, 136, last);
        put_u64(meta, 144, 1 - i as u64);
    }
    Ok(builder.pages.concat())
}
/// One named table of a bounded regular file, read without
/// opening LMDB locks or changing any database files.
pub fn read_table(path: &str, name: &str) -> Result<Vec<Record>> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!(crate::sysutil::Error::os("open", path, &e)))?;
    let info = file
        .metadata()
        .map_err(|e| anyhow::anyhow!(crate::sysutil::Error::os("stat", path, &e)))?;
    ensure!(
        info.is_file() && info.len() <= MAX_FILE_BYTES as u64,
        "LMDB input is outside probe bounds"
    );
    let mut data = vec![0; info.len() as usize];
    file.read_exact_at(&mut data, 0).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            anyhow::anyhow!("EOF")
        } else {
            anyhow::anyhow!(crate::sysutil::Error::os("read", path, &error))
        }
    })?;
    Image::open(&data)?.table(name)
}
