//! SVD-file parser for register test generator.

use crate::{
    read_excludes_from_env, read_file_or_panic, read_vec_from_env, Access, AddrRepr, ArchiPtr,
    Error, IncompatibleTypesError, ItemFilter, NotImplementedError, PositionalError, Protection,
    PtrSize, RegPath, RegValue, Register, RegisterDimElementGroup, RegisterPropertiesGroup,
    Registers, ResetValue, SvdParseError,
};
use itertools::Itertools;
use log::{debug, info, warn};
use regex::Regex;
use roxmltree::{Document, Node};
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    env, panic,
    path::{Path, PathBuf},
    str::FromStr,
};

/// Find a child node with given tag name.
fn find_text_in_node_by_tag_name<'a>(
    node: &'a Node,
    tag: &str,
) -> Result<(&'a str, Node<'a, 'a>), PositionalError<SvdParseError>> {
    maybe_find_text_in_node_by_tag_name(node, tag).ok_or_else(|| {
        SvdParseError::ExpectedTagInElement {
            elem_name: node.tag_name().name().to_owned(),
            tag: tag.to_owned(),
        }
        .with_byte_pos_range(node.range(), node.document())
    })
}

/// Try to find a child node with given name.
fn maybe_find_text_in_node_by_tag_name<'a>(
    node: &'a Node,
    tag: &str,
) -> Option<(&'a str, Node<'a, 'a>)> {
    node.children()
        .find(|n| n.has_tag_name(tag))
        .map(|n| (n.text().expect("Node does not have text."), n))
}

/// Returns the appropriate multiplier for given character, represented by type parameter `P`
fn binary_size_mult_from_char<P: TryFrom<u64>>(c: char) -> Result<P, SvdParseError>
where
    SvdParseError: From<<P as TryFrom<u64>>::Error>,
{
    match c {
        'k' | 'K' => Ok(1024u64.try_into()?),
        'm' | 'M' => Ok((1024 * 1024u64).try_into()?),
        'g' | 'G' => Ok((1024 * 1024 * 1024u64).try_into()?),
        't' | 'T' => Ok((1024 * 1024 * 1024 * 1024u64).try_into()?),
        _ => Err(SvdParseError::InvalidSizeMultiplierSuffix(c)),
    }
}

#[test]
fn binary_size_mult_from_char_works() {
    // 32-bit
    assert_eq!(binary_size_mult_from_char('k'), Ok(1024u32));
    assert_eq!(binary_size_mult_from_char('m'), Ok(1024 * 1024u32));
    assert_eq!(binary_size_mult_from_char('g'), Ok(1024 * 1024 * 1024u32));
    assert!(binary_size_mult_from_char::<u32>('t').is_err());

    // 64-bit
    assert_eq!(binary_size_mult_from_char('k'), Ok(1024u64));
    assert_eq!(binary_size_mult_from_char('m'), Ok(1024 * 1024u64));
    assert_eq!(binary_size_mult_from_char('g'), Ok(1024 * 1024 * 1024u64));
    assert_eq!(
        binary_size_mult_from_char('t'),
        Ok(1024 * 1024 * 1024 * 1024u64)
    );
}

/// Parses an integer from `text`
///
/// This implementation is format aware and uses regex to ensure correct behavior.
fn parse_nonneg_int<P: ArchiPtr>(text: &str) -> Result<P, SvdParseError>
where
    SvdParseError: From<<P as num::Num>::FromStrRadixErr>
        + From<<P as FromStr>::Err>
        + From<<P as TryFrom<u64>>::Error>,
{
    // Compile Regexes only once as recommended by the documentation of the Regex crate
    use lazy_static::lazy_static;
    lazy_static! {
        // [0x|0X|\#]{1}          # hexadecimal prefix
        /// Regular expression to capture hexadecimal numbers, as defined in CMSIS-SVD schema
        static ref HEX_NONNEG_INT_RE: Regex = Regex::new(
            r"(?x)              # insignificant whitespace
            \+?                 # zero or one plus sign
            (?:0x|0X|\#)        # hexadecimal prefix
            ([[:xdigit:]]+)     # one or more hexadecimal digits (captured as #1)
            [[:space:]]?        # zero or one of whitespace
            ([kmgtKMGT])?       # zero or one of kilo, mega, giga, tera identifier (captured as #2)
        ").unwrap();

        /// Regular expression to capture decimal numbers, as defined in CMSIS-SVD schema
        static ref DEC_NONNEG_INT_RE: Regex = Regex::new(
            r"(?x)              # insignificant whitespace
            \+?                 # zero or one plus sign
            ([[:digit:]]+)      # one or more decimal digits (captured as #1)
            [[:space:]]?        # zero or one of whitespace
            ([kmgtKMGT])?       # zero or one of kilo, mega, giga, tera identifier (captured as #2)
        ").unwrap();
    }

    // Pick either hexadecimal or decimal format based on which fits
    // TODO: pick binary format on '#'

    let (number_part, size_mult_capture) = if HEX_NONNEG_INT_RE.is_match(text) {
        // Safety: we checked above that at least one match exists in text
        let captures = HEX_NONNEG_INT_RE.captures_iter(text).next().unwrap();

        let digits = &captures[1];
        let number = P::from_str_radix(digits, 16)?;

        let size_mult = captures.get(2);
        (number, size_mult)
    } else if DEC_NONNEG_INT_RE.is_match(text) {
        // Safety: we checked above that at least one match exists in text
        let captures = DEC_NONNEG_INT_RE.captures_iter(text).next().unwrap();

        let digits = &captures[1];
        let number = digits.parse::<P>()?;

        let size_mult = captures.get(2);
        (number, size_mult)
    } else {
        return Err(SvdParseError::InvalidNonnegInt(text.to_owned()));
    };

    let size_mult: Option<P> = size_mult_capture
        // Safety: we know from the regex that there is only one possible size mult char
        .map(|s| s.as_str().chars().next().unwrap())
        .map(|c| binary_size_mult_from_char(c))
        .transpose()?;

    Ok(match size_mult {
        Some(mult) => number_part * mult,
        None => number_part,
    })
}

#[test]
fn parse_nonneg_int_works() {
    assert_eq!(
        parse_nonneg_int::<u32>("0xFFB00000").unwrap(),
        0xFFB0_0000u32
    );
    assert_eq!(
        parse_nonneg_int::<u32>("+0xFFB00000").unwrap(),
        0xFFB0_0000u32
    );
    // TODO: this test case is invalid. # means binary not hex, and the parser is faulty
    assert_eq!(
        parse_nonneg_int::<u32>("#FFB00000").unwrap(),
        0xFFB0_0000u32
    );
    assert_eq!(parse_nonneg_int::<u32>("42").unwrap(), 42u32);
    assert_eq!(parse_nonneg_int::<u32>("1 k").unwrap(), 1024u32);
    assert_eq!(
        parse_nonneg_int::<u32>("437260288").unwrap(),
        437_260_288u32
    );
}

#[derive(Clone, Default)]
struct RegPropGroupBuilder {
    /// Register bit-width.
    pub size: Option<PtrSize>,
    /// Register access rights.
    pub access: Option<Access>,
    /// Register access privileges.
    pub protection: Option<Protection>,
    /// Register value after reset.
    /// Actual reset value is calculated using reset value and reset mask.
    pub(crate) reset_value: Option<RegValue>,
    /// Register bits with defined reset value are marked as high.
    pub(crate) reset_mask: Option<RegValue>,
}

/// Add text position information to an [`SvdParseError`] converting it into a [`PositionalError`]
fn err_with_pos(e: impl Into<SvdParseError>, node: &Node) -> PositionalError<SvdParseError> {
    e.into().with_byte_pos_range(node.range(), node.document())
}

/// Finds a property from `node` by `tag`, calling `process` for its contents if present
///
/// Returns an error if value cannot be parsed in a legal way.
///
/// # Arguments
///
/// * `tag`     - The tag to locate within `node`
/// * `node`    - The node to search (does not recurse)
/// * `process` - The function to call for the found property
fn process_prop_from_node_if_present<T, F>(
    tag: &str,
    node: &Node,
    process: F,
) -> Result<Option<T>, PositionalError<SvdParseError>>
where
    F: Fn(&str) -> Result<T, SvdParseError>,
{
    maybe_find_text_in_node_by_tag_name(node, tag)
        .map(|(s, prop_node)| process(s).map_err(|e| err_with_pos(e, &prop_node)))
        .transpose()
}

impl RegPropGroupBuilder {
    /// Returns a new [`RegPropGroupBuilder`] with applicable attributes from `node`
    ///
    /// Reads the following attributes from `node`:
    ///
    /// * size
    /// * access
    /// * resetValue
    /// * resetMask
    ///
    /// If a value was not available, the respective field is set to None.
    fn try_from_node(node: &Node) -> Result<Self, PositionalError<SvdParseError>> {
        let mut properties = Self::default();
        properties.update_from_node(node)?;
        Ok(properties)
    }

    /// Inherit properties from parent and update with current node's properties if defined.
    ///
    /// # Arguments
    ///
    /// * `node` - can be either cluster or register node
    fn clone_and_update_from_node(
        &self,
        node: &Node,
    ) -> Result<Self, PositionalError<SvdParseError>> {
        let mut properties = self.clone();
        properties.update_from_node(node)?;
        Ok(properties)
    }

    /// Update properties for this [`RegPropGroupBuilder`] where present
    ///
    /// # Arguments
    ///
    /// * `node` - can be either peripheral, cluster, or register node
    ///
    /// Updates the following attributes from `node`:
    ///
    /// * size
    /// * access
    /// * resetValue
    /// * resetMask
    fn update_from_node(&mut self, node: &Node) -> Result<(), PositionalError<SvdParseError>> {
        if let Some(size) = process_prop_from_node_if_present("size", node, |s| {
            let bit_count = s.parse()?;
            PtrSize::from_bit_count(bit_count).ok_or(SvdParseError::BitCountToPtrWidth(bit_count))
        })? {
            self.size = Some(size);
        }
        if let Some(access) = process_prop_from_node_if_present("access", node, |s| {
            Access::from_str(s).map_err(|e| e.into())
        })? {
            self.access = Some(access);
        };
        if let Some(protection) =
            process_prop_from_node_if_present("protection", node, |s| Protection::from_str(s))?
        {
            self.protection = Some(protection);
        };
        if let Some(reset_value) = process_prop_from_node_if_present("resetValue", node, |s| {
            parse_nonneg_int(s).map(RegValue::U64)
        })? {
            self.reset_value = Some(reset_value);
        };
        if let Some(reset_mask) = process_prop_from_node_if_present("resetMask", node, |s| {
            parse_nonneg_int(s).map(RegValue::U64)
        })? {
            self.reset_mask = Some(reset_mask);
        };
        Ok(())
    }

    pub(crate) fn build(
        self,
        reg_path: &str,
    ) -> Result<RegisterPropertiesGroup, IncompatibleTypesError> {
        let value_size = self.size.unwrap_or_else(|| {
            warn!("property 'size' is not defined for register '{reg_path}' or any of its parents, assuming size = u32");
            PtrSize::U32
        });
        let access = self.access.unwrap_or_else(|| {
            warn!("property 'access' is not defined for register '{reg_path}' or any of its parents, assuming access = read-write");
            Access::ReadWrite
        });
        let protection = self.protection.unwrap_or_else(|| {
            // This is a very common omission from SVD. We should not warn about it unless required by user
            // TODO: allow changing this to warn! or error! via top level config
            debug!("property 'protection' is not defined for register '{reg_path}' or any of its parents, assuming protection = NonSecureOrSecure");
            Protection::NonSecureOrSecure
        });
        let reset_value = {
            let reset_value = self.reset_value.unwrap_or_else(|| {
                warn!("property 'resetValue' is not defined for register '{reg_path}' or any of its parents, assuming resetValue = 0");
                value_size.zero_value()
            });
            let reset_mask = self.reset_mask.unwrap_or_else(|| {
                warn!("property 'resetMask' is not defined for register '{reg_path}' or any of its parents, assuming resetMask = {}::MAX", value_size);
                value_size.max_value()
            });
            ResetValue::with_mask(reset_value, reset_mask)?
        };

        Ok(RegisterPropertiesGroup::new(
            value_size,
            access,
            protection,
            reset_value,
        ))
    }
}

// The presence of this pattern in the register name likely indicates that this
// is an array register
//
// TODO: should use a more robust way of detecting arrays, i.e., checking the
// fields for the reg in question
const SVD_ARRAY_REPETITION_PATTERN: &str = "%s";

enum RegisterParentKind<P: ArchiPtr> {
    Periph,
    Cluster {
        cluster_name: String,
        cluster_offset: P,
    },
}

struct RegisterParent<P: ArchiPtr> {
    kind: RegisterParentKind<P>,
    periph_name: String,
    periph_base: P,
    properties: RegPropGroupBuilder,
}

impl<P: ArchiPtr> RegisterParent<P>
where
    SvdParseError: From<<P as num::Num>::FromStrRadixErr>
        + From<<P as FromStr>::Err>
        + From<<P as TryFrom<u64>>::Error>,
{
    fn from_periph_node(periph_node: &Node) -> Result<Self, PositionalError<SvdParseError>> {
        let (base_addr_str, base_addr_node) =
            find_text_in_node_by_tag_name(periph_node, "baseAddress")?;
        let base_addr =
            parse_nonneg_int(base_addr_str).map_err(|e| err_with_pos(e, &base_addr_node))?;
        let (periph_name, _) = find_text_in_node_by_tag_name(periph_node, "name")?;

        Ok(Self {
            periph_name: periph_name.to_string(),
            periph_base: base_addr,
            properties: RegPropGroupBuilder::try_from_node(periph_node)?,
            kind: RegisterParentKind::Periph,
        })
    }

    fn clone_and_update_from_cluster(
        &self,
        cluster_node: &Node,
    ) -> Result<Self, PositionalError<SvdParseError>> {
        let cluster_name = find_text_in_node_by_tag_name(cluster_node, "name")?
            .0
            .to_owned();
        let (cluster_offset_str, cluster_offset_node) =
            find_text_in_node_by_tag_name(cluster_node, "addressOffset")?;
        let cluster_offset = parse_nonneg_int(cluster_offset_str)
            .map_err(|e| err_with_pos(e, &cluster_offset_node))?;

        Ok(Self {
            periph_name: self.periph_name.clone(),
            periph_base: self.periph_base.clone(),
            properties: self.properties.clone_and_update_from_node(cluster_node)?,
            kind: RegisterParentKind::Cluster {
                cluster_name,
                cluster_offset,
            },
        })
    }
}

impl TryFrom<&Node<'_, '_>> for RegisterDimElementGroup {
    type Error = PositionalError<SvdParseError>;

    fn try_from(value: &Node) -> Result<Self, Self::Error> {
        let (dim, dim_node) = find_text_in_node_by_tag_name(value, "dim")?;
        let dim = parse_nonneg_int(dim).map_err(|e| err_with_pos(e, &dim_node))?;
        let (dim_inc, dim_inc_node) = find_text_in_node_by_tag_name(value, "dimIncrement")?;
        let dim_increment =
            parse_nonneg_int(dim_inc).map_err(|e| err_with_pos(e, &dim_inc_node))?;
        Ok(Self { dim, dim_increment })
    }
}

fn process_register<P: ArchiPtr>(
    parent: &RegisterParent<P>,
    register_node: Node,
    reg_filter: &ItemFilter<String>,
    syms_regex: &ItemFilter<String>,
) -> Result<Option<Register<P>>, PositionalError<SvdParseError>>
where
    SvdParseError: From<<P as num::Num>::FromStrRadixErr>
        + From<<P as FromStr>::Err>
        + From<<P as TryFrom<u64>>::Error>,
{
    let name = find_text_in_node_by_tag_name(&register_node, "name")?
        .0
        .to_owned();
    let (addr_offset_str, addr_offset_node) =
        find_text_in_node_by_tag_name(&register_node, "addressOffset")?;
    let addr_offset =
        parse_nonneg_int(addr_offset_str).map_err(|e| err_with_pos(e, &addr_offset_node))?;

    //let reg_name = remove_illegal_characters(reg_name);
    let path = RegPath::from_components(
        parent.periph_name.clone(),
        match &parent.kind {
            RegisterParentKind::Periph => None,
            RegisterParentKind::Cluster { cluster_name, .. } => Some(cluster_name.clone()),
        },
        name.clone(),
    );
    let reg_path = path.join("-");

    if syms_regex.is_blocked(&reg_path) {
        info!("Register {reg_path} was not included due to regex set in SYMS_REGEX");
        return Ok(None);
    }

    // FIXME: we match against only the register's name, not the path. This is not a
    // great way to exclude registers. We should match against the entire path.
    if reg_filter.is_blocked(&name) {
        info!("register {name} is was not included due to values set in PATH_EXCLUDES");
        return Ok(None);
    }

    if name.contains(SVD_ARRAY_REPETITION_PATTERN) {
        warn!("{}, skipping", NotImplementedError::SvdArray(reg_path));
        return Ok(None);
    }

    let properties = parent
        .properties
        .clone_and_update_from_node(&register_node)?;
    let properties = properties
        .build(&reg_path)
        .map_err(|e| err_with_pos(e, &register_node))?;

    let addr = AddrRepr::<P>::new(
        parent.periph_base.clone(),
        match &parent.kind {
            RegisterParentKind::Periph => None,
            RegisterParentKind::Cluster { cluster_offset, .. } => Some(cluster_offset.clone()),
        },
        addr_offset,
    );
    let dimensions = RegisterDimElementGroup::try_from(&register_node).ok();

    let register = Register {
        path,
        addr,
        properties,
        dimensions,
    };
    Ok(Some(register))
}

fn process_cluster<P: ArchiPtr>(
    parent: &RegisterParent<P>,
    cluster_node: Node,
    reg_filter: &ItemFilter<String>,
    syms_regex: &ItemFilter<String>,
) -> Result<Option<Vec<Register<P>>>, PositionalError<SvdParseError>>
where
    SvdParseError: From<<P as num::Num>::FromStrRadixErr>
        + From<<P as FromStr>::Err>
        + From<<P as TryFrom<u64>>::Error>,
{
    let current_parent = parent.clone_and_update_from_cluster(&cluster_node)?;

    let mut registers = Vec::new();
    for register_node in cluster_node
        .children()
        .filter(|n| n.has_tag_name("register"))
    {
        if let Some(register) =
            process_register(&current_parent, register_node, reg_filter, syms_regex)?
        {
            registers.push(register);
        }
    }
    Ok(Some(registers))
}

fn process_peripheral<P: ArchiPtr>(
    periph_node: Node,
    periph_filter: &ItemFilter<String>,
    reg_filter: &ItemFilter<String>,
    syms_regex: &ItemFilter<String>,
) -> Result<Option<Vec<Register<P>>>, PositionalError<SvdParseError>>
where
    SvdParseError: From<<P as num::Num>::FromStrRadixErr>
        + From<<P as FromStr>::Err>
        + From<<P as TryFrom<u64>>::Error>,
{
    let periph = RegisterParent::from_periph_node(&periph_node)?;
    let periph_name = &periph.periph_name;

    if periph_filter.is_blocked(&periph_name.to_lowercase()) {
        info!("Peripheral {periph_name} was not included due to values set in INCLUDE_PERIPHERALS and/or EXCLUDE_PERIPHERALS");
        return Ok(None);
    }

    let registers_nodes = periph_node
        .children()
        .filter(|n| n.has_tag_name("registers"))
        .collect_vec();
    assert!(
        registers_nodes.len() == 1,
        "SVD file peripheral node must contains one registers node."
    );
    let registers_node = registers_nodes.first().unwrap();

    let mut registers = Vec::new();
    for cluster_node in registers_node
        .children()
        .filter(|n| n.has_tag_name("cluster"))
    {
        if let Some(cluster_registers) =
            process_cluster(&periph, cluster_node, reg_filter, syms_regex)?
        {
            registers.extend(cluster_registers);
        }
    }
    for register_node in registers_node
        .children()
        .filter(|n| n.has_tag_name("register"))
    {
        if let Some(register) = process_register(&periph, register_node, reg_filter, syms_regex)? {
            registers.push(register);
        }
    }
    Ok(Some(registers))
}

/// Find registers from SVD XML-document.
fn find_registers<P: ArchiPtr>(
    parsed: &Document,
    reg_filter: &ItemFilter<String>,
    periph_filter: &ItemFilter<String>,
    syms_regex: &ItemFilter<String>,
) -> Result<Registers<P>, PositionalError<SvdParseError>>
where
    SvdParseError: From<<P as num::Num>::FromStrRadixErr>
        + From<<P as FromStr>::Err>
        + From<<P as TryFrom<u64>>::Error>,
{
    let device_nodes = parsed
        .root()
        .children()
        .filter(|n| n.has_tag_name("device"))
        .collect_vec();
    assert!(
        device_nodes.len() == 1,
        "SVD file must contain one device node."
    );
    let device_node = device_nodes.first().unwrap();

    let peripherals_nodes = device_node
        .children()
        .filter(|n| n.has_tag_name("peripherals"))
        .collect_vec();
    assert!(
        peripherals_nodes.len() == 1,
        "SVD file must contains one peripherals node."
    );
    let peripherals_node = peripherals_nodes.first().unwrap();

    let mut registers = Vec::new();
    for peripheral_node in peripherals_node
        .children()
        .filter(|n| n.has_tag_name("peripheral"))
    {
        if let Some(peripheral_registers) =
            process_peripheral(peripheral_node, periph_filter, reg_filter, syms_regex)?
        {
            registers.extend(peripheral_registers);
        }
    }

    let mut peripherals = HashSet::new();
    let mut addresses = HashMap::new();
    for register in &registers {
        peripherals.insert(register.path.periph.clone());
        if let Entry::Vacant(entry) = addresses.entry(register.full_addr().unwrap()) {
            entry.insert(register.path.join("-"));
        } else {
            let address_holder = addresses
                .get(&register.full_addr().unwrap())
                .expect("failed to find register name by key");
            warn!("register {}'s full address is already taken by register {address_holder}. This register is ignored.", register.path.join("-"));
        }
    }

    info!("Found {} peripherals:", peripherals.len());
    for peripheral in peripherals {
        info!("    {peripheral}");
    }
    Ok(registers.into())
}

/// Parse the file at `svd_path` into a list of registers with provided filters & constraints
///
/// # Arguments
///
/// * `svd_path`        - The path to the SVD file
/// * `reg_filter`      - What registers to include or exclude
/// * `periph_filter`   - What peripherals to include or exclude
/// * `syms_filter` -   - What symbols to include or exclude (applying to full register identifier)
fn parse_svd_into_registers<P: ArchiPtr>(
    svd_path: &Path,
    reg_filter: &ItemFilter<String>,
    periph_filter: &ItemFilter<String>,
    syms_filter: &ItemFilter<String>,
) -> Result<Registers<P>, Error>
where
    SvdParseError: From<<P as num::Num>::FromStrRadixErr>
        + From<<P as FromStr>::Err>
        + From<<P as TryFrom<u64>>::Error>,
{
    let svd_content = read_file_or_panic(svd_path);

    let parsed = Document::parse(&svd_content).expect("Failed to parse SVD content.");
    let registers = find_registers(&parsed, reg_filter, periph_filter, syms_filter)
        .map_err(|positional| positional.with_fname(format!("{}", svd_path.display())))?;

    // If zero registers were chosen for generation, this run is useless.
    // Therefore we treat it as an error.
    // TODO: allow ignoring this error for special cases with a suitable flag on Config-struct
    if registers.is_empty() {
        return Err(Error::ZeroEntries);
    }

    info!("Found {} registers.", registers.len());
    Ok(registers)
}

/// Parse SVD-file.
///
/// # Panics
///
/// - Missing path to SVD-file
///
/// # Errors
///
/// - Failed to interpret given options
/// - Failed to parse given SVD file
pub fn parse<P: ArchiPtr>() -> Result<Registers<P>, Error>
where
    SvdParseError: From<<P as num::Num>::FromStrRadixErr>
        + From<<P as FromStr>::Err>
        + From<<P as TryFrom<u64>>::Error>,
{
    // TODO: remove panic, use error
    // TODO: remove path_svd
    let svd_path = env::var("SVD_PATH").unwrap_or_else(|_| {
        env::var("PATH_SVD").map_or_else(
            |err| panic!("PATH_SVD or SVD_PATH must be set: {err}"),
            |p| {
                warn!("PATH_SVD is under threat of deprecation, use SVD_PATH instead");
                p
            },
        )
    });
    let include_peripherals = read_vec_from_env("INCLUDE_PERIPHERALS", ',');
    let exclude_peripherals = read_vec_from_env("EXCLUDE_PERIPHERALS", ',');
    let periph_filter =
        ItemFilter::list(include_peripherals, exclude_peripherals.unwrap_or_default());
    let include_syms_regex = env::var("INCLUDE_SYMS_REGEX")
        .ok()
        .map(|s| Regex::new(&s))
        .transpose()?;
    let exclude_syms_regex = env::var("EXCLUDE_SYMS_REGEX")
        .ok()
        .map(|s| Regex::new(&s))
        .transpose()?;
    let syms_filter = ItemFilter::regex(include_syms_regex, exclude_syms_regex);
    let reg_filter = ItemFilter::list(None, read_excludes_from_env().unwrap_or_default());
    parse_svd_into_registers(
        &PathBuf::from(svd_path),
        &reg_filter,
        &periph_filter,
        &syms_filter,
    )
}
