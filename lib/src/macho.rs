use crate::error::{GetSymbolsError, Result};
use crate::shared::{
    object_to_map, FileAndPathHelper, FileContents, FileContentsWrapper, SymbolicationQuery,
    SymbolicationResult,
};
use crate::{
    dwarf::{collect_dwarf_address_debug_data, AddressPair},
    shared::RangeReadRef,
};
use object::{
    macho::{MachHeader32, MachHeader64},
    ReadRef,
};
use object::{
    read::macho::{FatArch, MachHeader},
    Endianness,
};
use object::{
    read::{archive::ArchiveFile, File, FileKind, Object, ObjectSymbol},
    ObjectMapEntry,
};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Returns the (offset, size) in the fat binary file for the object that matches
// breakpad_id, if found.
pub fn get_arch_range(
    file_contents: &FileContentsWrapper<impl FileContents>,
    arches: &[impl FatArch],
    breakpad_id: &str,
) -> Result<(u64, u64)> {
    let mut uuids = Vec::new();
    let mut errors = Vec::new();

    for fat_arch in arches {
        let range = fat_arch.file_range();
        let (start, size) = range;
        match get_macho_uuid(file_contents.range(start, size)) {
            Ok(uuid) => {
                if uuid == breakpad_id {
                    return Ok(range);
                }
                uuids.push(uuid);
            }
            Err(err) => {
                errors.push(err);
            }
        }
    }
    Err(GetSymbolsError::NoMatchMultiArch(uuids, errors))
}

pub fn get_macho_uuid<'a, R: ReadRef<'a>>(data: R) -> Result<String> {
    let helper = || {
        let file_kind = FileKind::parse(data)?;
        match file_kind {
            FileKind::MachO32 => {
                let header = MachHeader32::<Endianness>::parse(data)?;
                header.uuid(header.endian()?, data)
            }
            FileKind::MachO64 => {
                let header = MachHeader64::<Endianness>::parse(data)?;
                header.uuid(header.endian()?, data)
            }
            _ => panic!("Unexpected file kind {:?}", file_kind),
        }
    };
    match helper() {
        Ok(Some(uuid)) => Ok(format!("{:X}0", Uuid::from_bytes(uuid).to_simple())),
        Ok(None) => Err(GetSymbolsError::InvalidInputError("Missing mach-o uuid")),
        Err(err) => Err(GetSymbolsError::MachOHeaderParseError(err)),
    }
}

pub async fn get_symbolication_result<'a, 'b, R>(
    file_contents: FileContentsWrapper<impl FileContents>,
    file_range: Option<(u64, u64)>,
    query: SymbolicationQuery<'a>,
    helper: &impl FileAndPathHelper,
) -> Result<R>
where
    R: SymbolicationResult,
{
    let file_contents_ref = &file_contents;
    let range = match file_range {
        Some((start, size)) => file_contents_ref.range(start, size),
        None => file_contents_ref.full_range(),
    };

    let uuid = get_macho_uuid(range)?;
    if uuid != query.breakpad_id {
        return Err(GetSymbolsError::UnmatchedBreakpadId(
            uuid,
            query.breakpad_id.to_string(),
        ));
    }

    let macho_file = File::parse(range).map_err(|e| GetSymbolsError::MachOHeaderParseError(e))?;
    let map = object_to_map(&macho_file);
    let addresses = query.addresses;
    let mut symbolication_result = R::from_full_map(map, addresses);

    if !R::result_kind().wants_debug_info_for_addresses() {
        return Ok(symbolication_result);
    }

    // We need to gather debug info for the supplied addresses.
    // On macOS, debug info can either be in this macho_file, or it can be in
    // other files ("external objects").
    // The following code is written in a way that handles a mixture of the two;
    // it gathers what debug info it can find in the current object, and also
    // gathers external object references. Then it reads those external objects
    // and keeps gathering more data until it has it all.
    // In theory, external objects can reference other external objects. The code
    // below handles such nesting, but it's unclear if this can happen in practice.

    // The original addresses which our caller wants to look up are relative to
    // the "root object". In the external objects they'll be at a different address.
    // To correctly associate the found information with the original address,
    // we need to track an AddressPair, which has both the original address and
    // the address that we need to look up in the current object.

    let addresses_in_root_object = make_address_pairs_for_root_object(addresses, &macho_file);
    let mut object_references = VecDeque::new();
    collect_debug_info_and_object_references(
        &macho_file,
        &addresses_in_root_object,
        &mut symbolication_result,
        &mut object_references,
    )?;

    // We are now done with the "root object" and can discard its data.
    drop(macho_file);
    drop(file_contents);

    // Collect the debug info from the external references.
    traverse_object_references_and_collect_debug_info(
        object_references,
        &mut symbolication_result,
        helper,
    )
    .await?;

    Ok(symbolication_result)
}

async fn traverse_object_references_and_collect_debug_info(
    object_references: VecDeque<ObjectReference>,
    symbolication_result: &mut impl SymbolicationResult,
    helper: &impl FileAndPathHelper,
) -> Result<()> {
    // Do a breadth-first-traversal of the external debug info reference tree.
    // We do this using a while loop and a VecDeque rather than recursion, because
    // async functions can't easily recurse.
    let mut remaining_object_references = object_references;
    while let Some(obj_ref) = remaining_object_references.pop_front() {
        let path = obj_ref.path();
        let file_contents = match helper.open_file(path).await {
            Ok(data) => FileContentsWrapper::new(data),
            Err(_) => {
                // We probably couldn't find the file, but that's fine.
                // It would be good to collect this error somewhere.
                continue;
            }
        };

        for (data, functions) in obj_ref.into_objects(&file_contents)?.into_iter() {
            let macho_file =
                File::parse(data).or_else(|x| Err(GetSymbolsError::MachOHeaderParseError(x)))?;
            let addresses_in_this_object = translate_addresses_to_object(&macho_file, functions);
            collect_debug_info_and_object_references(
                &macho_file,
                &addresses_in_this_object,
                symbolication_result,
                &mut remaining_object_references,
            )?;
        }
    }

    Ok(())
}

fn make_address_pairs_for_root_object<'data: 'file, 'file, O>(
    addresses: &[u32],
    macho_file: &'file O,
) -> Vec<AddressPair>
where
    O: Object<'data, 'file>,
{
    // Make an AddressPair for every address.
    // You'd think that this is the root address, we'd just have pairs of
    // identical addresses here. However, AddressPair also encodes the difference
    // between "symbol table address space" and "DWARF debug info address space".

    // For executable files, there's a 0x100000000 offset between the addresses
    // that are used in the symbol table and the addresses that are used in the
    // debug info. This 0x100000000 is the vmaddr of the text section.
    // I'm not sure what the names of those two spaces are, and I can't give a
    // better explanation than "the code below seems to work".
    // For shared libraries, vmaddr_of_text_segment is 0, in my experience.
    use object::read::ObjectSegment;
    let vmaddr_of_text_segment = macho_file
        .segments()
        .find(|segment| segment.name() == Ok(Some("__TEXT")))
        .map(|segment| segment.address())
        .unwrap_or(0);

    addresses
        .iter()
        .map(|a| AddressPair {
            original_address: *a,
            address_in_this_object: vmaddr_of_text_segment + *a as u64,
        })
        .collect()
}

struct FunctionsWithAddresses {
    /// Keys are byte strings of the function name.
    /// Values are the addresses under that function.
    map: HashMap<Vec<u8>, Vec<AddressWithOffset>>,
}

impl FunctionsWithAddresses {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn insert(&mut self, symbol_name: &[u8], addresses: Vec<AddressWithOffset>) {
        self.map.insert(symbol_name.to_owned(), addresses);
    }

    pub fn get_and_remove_addresses_for_function(
        &mut self,
        symbol_name: &str,
    ) -> Option<Vec<AddressWithOffset>> {
        self.map.remove(symbol_name.as_bytes())
    }
}

fn translate_addresses_to_object<'data: 'file, 'file, O>(
    macho_file: &'file O,
    mut functions: FunctionsWithAddresses,
) -> Vec<AddressPair>
where
    O: Object<'data, 'file>,
{
    let mut addresses_in_this_object = Vec::new();
    for symbol in macho_file.symbols() {
        if let Ok(symbol_name) = symbol.name() {
            if let Some(addresses) = functions.get_and_remove_addresses_for_function(symbol_name) {
                for AddressWithOffset {
                    original_address,
                    offset_from_function_start,
                } in addresses
                {
                    let address_in_this_object =
                        symbol.address() + offset_from_function_start as u64;
                    addresses_in_this_object.push(AddressPair {
                        original_address,
                        address_in_this_object,
                    });
                }
            }
        }
    }
    addresses_in_this_object.sort_by_key(|ap| ap.address_in_this_object);
    addresses_in_this_object
}

enum ObjectReference {
    Regular {
        path: PathBuf,
        functions: FunctionsWithAddresses,
    },
    Archive {
        path: PathBuf,
        archive_info: HashMap<String, FunctionsWithAddresses>,
    },
}

impl ObjectReference {
    fn path(&self) -> &Path {
        match self {
            ObjectReference::Regular { path, .. } => path,
            ObjectReference::Archive { path, .. } => path,
        }
    }

    fn into_objects<'a, 'b, R: ReadRef<'b>>(
        self,
        data: R,
    ) -> Result<Vec<(RangeReadRef<'b, R>, FunctionsWithAddresses)>> {
        match self {
            ObjectReference::Regular { functions, .. } => Ok(vec![(
                RangeReadRef::new(data, 0, data.len().unwrap_or(0)),
                functions,
            )]),
            ObjectReference::Archive {
                path, archive_info, ..
            } => {
                let archive = ArchiveFile::parse(data).or_else(|x| {
                    Err(GetSymbolsError::ArchiveParseError(
                        path.clone(),
                        Box::new(x),
                    ))
                })?;
                let archive_members_by_name: HashMap<Vec<u8>, (u64, u64)> = archive
                    .members()
                    .filter_map(|member| match member {
                        Ok(member) => Some((member.name().to_owned(), member.file_range())),
                        Err(_) => None,
                    })
                    .collect();
                let v: Vec<_> = archive_info
                    .into_iter()
                    .filter_map(|(name_in_archive, functions)| {
                        archive_members_by_name
                            .get(name_in_archive.as_bytes())
                            .map(|&(start, size)| (RangeReadRef::new(data, start, size), functions))
                    })
                    .collect();
                Ok(v)
            }
        }
    }
}

#[derive(Debug)]
struct ObjectMapSymbol<'a> {
    name: &'a [u8],
    address: u64,
    size: u64,
    object_index: usize,
}

/// addresses must be sorted by address_in_this_object
fn collect_debug_info_and_object_references<'data: 'file, 'file, 'a, O, R>(
    macho_file: &'file O,
    addresses: &[AddressPair],
    symbolication_result: &mut R,
    remaining_object_references: &mut VecDeque<ObjectReference>,
) -> Result<()>
where
    O: Object<'data, 'file>,
    R: SymbolicationResult,
{
    let object_map = macho_file.object_map();
    let objects = object_map.objects();
    let mut object_map_symbols: Vec<_> = object_map.symbols().to_owned();
    object_map_symbols.sort_by_key(|symbol| symbol.address());
    let functions_with_addresses = match_funs_to_addresses(&object_map_symbols[..], addresses);
    let mut external_funs_by_object: HashMap<usize, FunctionsWithAddresses> = HashMap::new();
    let mut original_addresses_found_in_external_objects = BTreeSet::new();
    for MatchedFunctionWithAddresses {
        object_index,
        fun_name,
        addresses,
    } in functions_with_addresses.into_iter()
    {
        for AddressWithOffset {
            original_address, ..
        } in &addresses
        {
            original_addresses_found_in_external_objects.insert(*original_address);
        }
        external_funs_by_object
            .entry(object_index)
            .or_insert_with(FunctionsWithAddresses::new)
            .insert(fun_name, addresses);
    }
    let internal_addresses: Vec<_> = addresses
        .iter()
        .cloned()
        .filter(|ap| !original_addresses_found_in_external_objects.contains(&ap.original_address))
        .collect();
    collect_dwarf_address_debug_data(macho_file, &internal_addresses, symbolication_result);

    let mut archives = HashMap::new();
    let mut regular_objects = HashMap::new();

    for (object_index, functions) in external_funs_by_object.into_iter() {
        let object_name = std::str::from_utf8(objects[object_index]).unwrap();
        match object_name.find('(') {
            Some(index) => {
                // This is an "archive" reference of the form
                // "/Users/mstange/code/obj-m-opt/toolkit/library/build/../../../js/src/build/libjs_static.a(Unified_cpp_js_src13.o)"
                let (path, paren_rest) = object_name.split_at(index);
                let path: PathBuf = path.into();
                let name_in_archive = paren_rest
                    .trim_start_matches('(')
                    .trim_end_matches(')')
                    .to_string();
                let archive_info = archives.entry(path).or_insert_with(HashMap::new);
                archive_info.insert(name_in_archive, functions);
            }
            None => {
                // This is a reference to a regular object file. Example:
                // "/Users/mstange/code/obj-m-opt/toolkit/library/build/../../components/sessionstore/Unified_cpp_sessionstore0.o"
                let path: PathBuf = object_name.into();
                regular_objects.insert(path, functions);
            }
        }
    }

    for (path, archive_info) in archives.into_iter() {
        remaining_object_references.push_back(ObjectReference::Archive { path, archive_info });
    }
    for (path, functions) in regular_objects.into_iter() {
        remaining_object_references.push_back(ObjectReference::Regular { path, functions });
    }

    Ok(())
}

#[derive(Debug)]
struct AddressWithOffset {
    original_address: u32,
    offset_from_function_start: u32,
}

struct MatchedFunctionWithAddresses<'a> {
    object_index: usize,
    fun_name: &'a [u8],
    addresses: Vec<AddressWithOffset>,
}

/// Assign each address to a function in an object.
/// function_symbols must be sorted by function.address().
/// addresses must be sorted by address_in_this_object.
/// This function is implemented as a linear single pass over both slices.
fn match_funs_to_addresses<'a, 'b, 'c>(
    function_symbols: &'a [ObjectMapEntry<'c>],
    addresses: &'b [AddressPair],
) -> Vec<MatchedFunctionWithAddresses<'c>> {
    let mut matches: Vec<MatchedFunctionWithAddresses> = Vec::new();
    let mut addr_iter = addresses.iter();
    let mut cur_addr = addr_iter.next();
    let mut fun_iter = function_symbols.iter();
    let mut cur_fun = fun_iter.next();
    let mut cur_fun_is_last_vec_element = false;
    while let (Some(address_pair), Some(fun)) = (cur_addr, cur_fun) {
        let original_address = address_pair.original_address;
        let address_in_this_object = address_pair.address_in_this_object;
        if !(fun.address() <= address_in_this_object) {
            // Advance address_pair.
            cur_addr = addr_iter.next();
            continue;
        }
        if !(address_in_this_object < fun.address() + fun.size()) {
            // Advance fun.
            cur_fun = fun_iter.next();
            cur_fun_is_last_vec_element = false;
            continue;
        }
        // Now the following is true:
        // fun.address() <= address_in_this_object && address_in_this_object < fun.address() + fun.size()
        let offset_from_function_start = (address_in_this_object - fun.address()) as u32;
        let address_with_offset = AddressWithOffset {
            original_address,
            offset_from_function_start,
        };
        if cur_fun_is_last_vec_element {
            matches
                .last_mut()
                .unwrap()
                .addresses
                .push(address_with_offset);
        } else {
            matches.push(MatchedFunctionWithAddresses {
                object_index: fun.object_index(),
                fun_name: fun.name(),
                addresses: vec![address_with_offset],
            });
            cur_fun_is_last_vec_element = true;
        }
        // Advance addr.
        cur_addr = addr_iter.next();
    }
    matches
}

#[derive(Debug)]
struct OriginObject<'a> {
    name: &'a str,
}

#[derive(Debug)]
struct Function<'a> {
    name: &'a str,
    address_range: std::ops::Range<u64>,
    object_index: Option<usize>,
}
