//! TinyBus ABI exports with explicit member lists for each provided interface.
//! The convenience export macro accepts one member list; this module provides
//! two objects, so it declares the manifest through the standard ABI types.
use std::sync::OnceLock;
use tinybus::module::{abi::*, manifest::*};
use tinybus::{BusName, InterfaceName, InterfaceVersion, MemberName, ObjectPath, Version};

#[unsafe(no_mangle)]
pub static TINYBUS_MODULE_ABI_V1: TbAbiDescriptor =
    TbAbiDescriptor::current(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

#[unsafe(no_mangle)]
pub extern "C" fn tinybus_module_manifest_v1() -> TbSlice {
    std::panic::catch_unwind(manifest).unwrap_or(TbSlice {
        ptr: std::ptr::null(),
        len: 0,
    })
}
fn manifest() -> TbSlice {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    let bytes = BYTES.get_or_init(|| {
        let version = Version::parse(env!("CARGO_PKG_VERSION")).expect("package semver");
        let provided = |interface: &str, methods: &[&str]| ProvidedInterface {
            version: InterfaceVersion::provided(
                InterfaceName::new(interface).expect("static interface"),
                version.clone(),
            ),
            methods: methods
                .iter()
                .map(|m| MemberName::new(*m).expect("static member"))
                .collect(),
            signals: vec![],
        };
        serde_json::to_vec(&ModuleManifest {
            schema: MANIFEST_SCHEMA,
            module: ModuleIdentity {
                name: env!("CARGO_PKG_NAME").into(),
                version: version.clone(),
                description: "Workflow authoring and schedule runtime".into(),
                homepage: None,
                license: env!("CARGO_PKG_LICENSE").into(),
            },
            bus_name: BusName::new(tinyflows_bus::BUS_NAME).expect("static bus name"),
            object_path: ObjectPath::new(tinyflows_bus::OBJECT_PATH).expect("static object path"),
            provides: vec![
                provided(tinyflows_bus::INTERFACE, tinyflows_bus::METHODS),
                provided(
                    tinyflows_bus::schedule::INTERFACE,
                    tinyflows_bus::schedule::METHODS,
                ),
            ],
            requires: vec![Dependency {
                interface: InterfaceVersion::consumed(
                    InterfaceName::new(tinyflows_bus::HOST_INTERFACE).expect("static interface"),
                    version.clone(),
                ),
                optional: true,
                reason: String::new(),
            }],
            environment: vec![],
            capabilities: vec![],
            lazy_init: false,
            worker_threads: 2,
            on_panic: PanicPolicy::Detach,
        })
        .expect("manifest serializes")
    });
    TbSlice {
        ptr: bytes.as_ptr(),
        len: bytes.len(),
    }
}

/// Start the standard TinyBus module runtime.
///
/// # Safety
/// `host` and `out` must satisfy the TinyBus ABI v1 pointer and lifetime contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tinybus_module_init_v1(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
) -> i32 {
    unsafe { tinybus_module::start_module(host, out, 2, true, super::service::setup) }
}
