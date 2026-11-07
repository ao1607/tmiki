//! The OpenRaster plugin as a development plugin: the same code as the
//! component, run natively over `tmiki plugin dev`'s RPC.
//!
//!     cargo build -p tmi-plugin-ora --features dev --bin ora-dev
//!     tmiki plugin dev target/debug/ora-dev -- add a.ora

fn main() {
    tmi_plugin_sdk::dev_main::<tmi_plugin_ora::plugin::Ora>()
}