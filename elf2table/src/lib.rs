//! Reads ELF metadata and builds an interner table

use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, ensure};
pub use defmt_decoder::Table;
use object::{Object, ObjectSection};
use log::*;

/// Parses an ELF file and returns the decoded `defmt` table
///
/// This function returns `None` if the ELF file contains no `.defmt` section
pub fn parse(elf: &[u8]) -> Result<Option<Table>, anyhow::Error> {
    println!("parse()");
    let elf = object::File::parse(elf)?;
    // find the index of the `.defmt` section
    let defmt_shndx = if let Some(section) = elf.section_by_name(".defmt") {
        section.index()
    } else {
        return Ok(None);
    };

    let mut map = BTreeMap::new();
    let mut version = None;
    let mut trace_start = None;
    let mut trace_end = None;
    let mut debug_start = None;
    let mut debug_end = None;
    let mut info_start = None;
    let mut info_end = None;
    let mut warn_start = None;
    let mut warn_end = None;
    let mut error_start = None;
    let mut error_end = None;
    for (_, entry) in elf.symbols() {
        let name = match entry.name() {
            Some(name) => name,
            None => continue,
        };

        // Not in the `.defmt` section because it's not tied to the address of any symbol
        // in `.defmt`.
        // Note that we check for a quoted and unquoted version symbol, since LLD has a bug that
        // makes it keep the quotes from the linker script.
        if name.starts_with("\"_defmt_version_ = ") || name.starts_with("_defmt_version_ = ") {
            let new_version = name
                .trim_start_matches("\"_defmt_version_ = ")
                .trim_start_matches("_defmt_version_ = ")
                .trim_end_matches('"');
            if let Some(version) = version {
                return Err(anyhow!(
                    "multiple defmt versions in use: {} and {} (only one is supported)",
                    version,
                    new_version
                ));
            }
            version = Some(new_version);
        }

        if entry.section_index() == Some(defmt_shndx) {
            match name {
                "_defmt_trace_start" => trace_start = Some(entry.address() as usize),
                "_defmt_trace_end" => trace_end = Some(entry.address() as usize),
                "_defmt_debug_start" => debug_start = Some(entry.address() as usize),
                "_defmt_debug_end" => debug_end = Some(entry.address() as usize),
                "_defmt_info_start" => info_start = Some(entry.address() as usize),
                "_defmt_info_end" => info_end = Some(entry.address() as usize),
                "_defmt_warn_start" => warn_start = Some(entry.address() as usize),
                "_defmt_warn_end" => warn_end = Some(entry.address() as usize),
                "_defmt_error_start" => error_start = Some(entry.address() as usize),
                "_defmt_error_end" => error_end = Some(entry.address() as usize),
                _ => {
                    map.insert(entry.address() as usize, name.to_string());
                }
            }
        }
    }

    // unify errors
    let (error, warn, info, debug, trace, version) = (|| -> Option<_> {
        Some((
            error_start?..error_end?,
            warn_start?..warn_end?,
            info_start?..info_end?,
            debug_start?..debug_end?,
            trace_start?..trace_end?,
            version?,
        ))
    })()
    .ok_or_else(|| anyhow!("`_defmt_*` symbol not found"))?;

    Table::new(map, debug, error, info, trace, warn, version)
        .map_err(anyhow::Error::msg)
        .map(Some)
}

#[derive(Clone)]
pub struct Location {
    pub file: PathBuf,
    pub line: u64,
    pub module: String,
}

impl fmt::Debug for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.file.display(), self.line)
    }
}

pub type Locations = BTreeMap<u64, Location>;

pub fn get_locations(elf: &[u8], table: &Table) -> Result<Locations, anyhow::Error> {
    println!("get_locations()");
    let live_syms = table.symbols().collect::<Vec<_>>();
    let object = object::File::parse(elf)?;
    let endian = if object.is_little_endian() {
        gimli::RunTimeEndian::Little
    } else {
        gimli::RunTimeEndian::Big
    };

    let load_section = |id: gimli::SectionId| {
        Ok(if let Some(s) = object.section_by_name(id.name()) {
            s.uncompressed_data().unwrap_or(Cow::Borrowed(&[][..]))
        } else {
            Cow::Borrowed(&[][..])
        })
    };
    let load_section_sup = |_| Ok(Cow::Borrowed(&[][..]));

    let dwarf_cow =
        gimli::Dwarf::<Cow<[u8]>>::load::<_, _, anyhow::Error>(&load_section, &load_section_sup)?;

    let borrow_section: &dyn for<'a> Fn(
        &'a Cow<[u8]>,
    ) -> gimli::EndianSlice<'a, gimli::RunTimeEndian> =
        &|section| gimli::EndianSlice::new(&*section, endian);

    let dwarf = dwarf_cow.borrow(&borrow_section);

    let mut units = dwarf.debug_info.units();

    let mut map = BTreeMap::new();
    while let Some(header) = units.next()? {
        let unit = dwarf.unit(header)?;
        let abbrev = header.abbreviations(&dwarf.debug_abbrev)?;

        let mut cursor = header.entries(&abbrev);

        ensure!(cursor.next_dfs()?.is_some(), "empty DWARF?");

        let mut segments = vec![];
        let mut depth = 0;
        while let Some((delta_depth, entry)) = cursor.next_dfs()? {
            depth += delta_depth;

            // NOTE .. here start the custom logic
            if entry.tag() == gimli::constants::DW_TAG_namespace {
                let mut attrs = entry.attrs();

                while let Some(attr) = attrs.next()? {
                    match attr.name() {
                        gimli::constants::DW_AT_name => {
                            if let gimli::AttributeValue::DebugStrRef(off) = attr.value() {
                                let s = dwarf.string(off)?;
                                for _ in (depth as usize)..segments.len() + 1 {
                                    segments.pop();
                                }
                                segments.push(core::str::from_utf8(&s)?.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            } else if entry.tag() == gimli::constants::DW_TAG_variable {
                // Iterate over the attributes in the DIE.
                let mut attrs = entry.attrs();

                // what we are after
                let mut decl_file = None;
                let mut decl_line = None; // line number
                let mut name = None;
                let mut linkage_name = None;
                let mut location = None;

                while let Some(attr) = attrs.next()? {
                    match attr.name() {
                        gimli::constants::DW_AT_name => {
                            if let gimli::AttributeValue::DebugStrRef(off) = attr.value() {
                                name = Some(off);
                            }
                        }

                        gimli::constants::DW_AT_decl_file => {
                            if let gimli::AttributeValue::FileIndex(idx) = attr.value() {
                                decl_file = Some(idx);
                            }
                        }

                        gimli::constants::DW_AT_decl_line => {
                            if let gimli::AttributeValue::Udata(line) = attr.value() {
                                decl_line = Some(line);
                            }
                        }

                        gimli::constants::DW_AT_location => {
                            if let gimli::AttributeValue::Exprloc(loc) = attr.value() {
                                location = Some(loc);
                            }
                        }

                        gimli::constants::DW_AT_linkage_name => {
                            if let gimli::AttributeValue::DebugStrRef(off) = attr.value() {
                                linkage_name = Some(off);
                            }
                        }

                        _ => {}
                    }
                }

                if let (
                    Some(name_index),
                    Some(linkage_name_index),
                    Some(file_index),
                    Some(line),
                    Some(loc),
                ) = (name, linkage_name, decl_file, decl_line, location)
                {
                    println!("found match?");
                    let name_slice = dwarf.string(name_index)?;
                    let name = core::str::from_utf8(&name_slice)?;
                    let linkage_name_slice = dwarf.string(linkage_name_index)?;
                    let linkage_name = core::str::from_utf8(&linkage_name_slice)?;
                    println!("MATCH? name={} linkage_name={}", name, linkage_name);

                    if name == "DEFMT_LOG_STATEMENT" {
                        println!("found defmt log statement");
                        // remove the `@` suffix
                        let linkage_name = linkage_name
                            .splitn(2, '@')
                            .next()
                            .ok_or_else(|| anyhow!("{} is missing `@` suffix", linkage_name))?;

                        if live_syms.contains(&linkage_name) {
                            println!("live sym: {}", name);
                            let addr = exprloc2address(unit.encoding(), &loc)?;
                            let file = file_index_to_path(file_index, &unit, &dwarf)?;
                            let module = segments.join("::");

                            let loc = Location { file, line, module };

                            if let Some(old) = map.insert(addr, loc.clone()) {
                                bail!("BUG in DWARF variable filter: index collision for addr 0x{:08x} (old = {:?}, new = {:?})", addr, old, loc);
                            }
                        } else {
                            println!("GCd sym: {}", name);
                            // this symbol was GC-ed by the linker (but remains in the DWARF info)
                            // so we discard it (its `addr` info is also wrong which causes collisions)
                        }
                    }
                }
            }
        }
    }

    Ok(map)
}

fn file_index_to_path<R>(
    index: u64,
    unit: &gimli::Unit<R>,
    dwarf: &gimli::Dwarf<R>,
) -> Result<PathBuf, anyhow::Error>
where
    R: gimli::read::Reader,
{
    ensure!(index != 0, "`FileIndex` was zero");

    let header = if let Some(program) = &unit.line_program {
        program.header()
    } else {
        bail!("no `LineProgram`");
    };

    let file = if let Some(file) = header.file(index) {
        file
    } else {
        bail!("no `FileEntry` for index {}", index)
    };

    let mut p = PathBuf::new();
    if let Some(dir) = file.directory(header) {
        let dir = dwarf.attr_string(unit, dir)?;
        let dir_s = dir.to_string_lossy()?;
        let dir = Path::new(&dir_s[..]);

        if !dir.is_absolute() {
            if let Some(ref comp_dir) = unit.comp_dir {
                p.push(&comp_dir.to_string_lossy()?[..]);
            }
        }
        p.push(&dir);
    }

    p.push(
        &dwarf
            .attr_string(unit, file.path_name())?
            .to_string_lossy()?[..],
    );

    Ok(p)
}

fn exprloc2address<R: gimli::read::Reader<Offset = usize>>(
    encoding: gimli::Encoding,
    data: &gimli::Expression<R>,
) -> Result<u64, anyhow::Error> {
    let mut pc = data.0.clone();
    while pc.len() != 0 {
        if let Ok(gimli::Operation::Address { address }) =
            gimli::Operation::parse(&mut pc, encoding)
        {
            return Ok(address);
        }
    }

    Err(anyhow!("`Operation::Address` not found"))
}
