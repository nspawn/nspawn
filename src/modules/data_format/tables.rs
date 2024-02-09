use crate::modules::data_structs::InterfacesConfigs;

use {
    crate::modules::{images::Images, machines::Machines, utilities::opt_string_eval},
    comfy_table::{presets::ASCII_FULL, Attribute, Cell, ContentArrangement, Table},
};

#[must_use]
pub fn create_images_table(images: Images, width: Option<u16>) -> Table {
    let mut table = Table::new();
    table
        .load_preset(ASCII_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            Cell::new("Name").add_attribute(Attribute::Bold),
            Cell::new("Type").add_attribute(Attribute::Bold),
            Cell::new("Read Only").add_attribute(Attribute::Bold),
            Cell::new("Usage").add_attribute(Attribute::Bold),
            Cell::new("Created").add_attribute(Attribute::Bold),
            Cell::new("Modified").add_attribute(Attribute::Bold),
        ]);

    // Only set the width if it's defined by the user
    if let Some(width) = width {
        table.set_width(width);
    }

    for mut image in images {
        table.add_row(vec![
            image.name.to_string(),
            image.type_field.to_string(),
            image.ro.to_string(),
            image.get_usage_in_gb(),
            image.created_to_timestamp(),
            image.modified_to_timestamp(),
        ]);
    }

    table
}

#[must_use]
pub fn create_machines_table(machines: Machines, width: Option<u16>) -> Table {
    let mut table = Table::new();
    table
        .load_preset(ASCII_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            Cell::new("MACHINE").add_attribute(Attribute::Bold),
            Cell::new("CLASS").add_attribute(Attribute::Bold),
            Cell::new("SERVICE").add_attribute(Attribute::Bold),
            Cell::new("OS").add_attribute(Attribute::Bold),
            Cell::new("VERSION").add_attribute(Attribute::Bold),
            Cell::new("ADDRESSES").add_attribute(Attribute::Bold),
        ]);

    // Only set the width if it's defined by the user
    if let Some(width) = width {
        table.set_width(width);
    }

    for machine in machines {
        table.add_row(vec![
            opt_string_eval(&machine.machine),
            opt_string_eval(&machine.class),
            opt_string_eval(&machine.service),
            opt_string_eval(&machine.os),
            opt_string_eval(&machine.version),
            opt_string_eval(&machine.addresses),
        ]);
    }

    table
}

#[must_use]
pub fn create_network_configs_table(_machine: &str, networks_config: &InterfacesConfigs) -> Table {
    let mut table = Table::new();

    table
        .load_preset(ASCII_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        // Add a header only with the machine name
        .set_header(vec![
            Cell::new("Machine").add_attribute(Attribute::Bold),
            Cell::new("Iface Family").add_attribute(Attribute::Bold),
            Cell::new("Iface Name").add_attribute(Attribute::Bold),
            Cell::new("IP Address").add_attribute(Attribute::Bold),
            Cell::new("Broadcast").add_attribute(Attribute::Bold),
            Cell::new("mtu").add_attribute(Attribute::Bold),
            Cell::new("Operstate").add_attribute(Attribute::Bold),
            Cell::new("Type").add_attribute(Attribute::Bold),
            Cell::new("MAC Address").add_attribute(Attribute::Bold),
        ]);

    for network in networks_config {
        for addrinfo in &network.addr_info {
            table.add_row(vec![
                _machine.to_string(),
                addrinfo.family.to_string(),
                network.ifname.to_string(),
                addrinfo.local.to_string(),
                addrinfo.broadcast.clone().unwrap_or_default(),
                network.mtu.to_string(),
                network.operstate.to_string(),
                network.link_type.to_string(),
                network.address.to_string(),
            ]);
        }
    }

    table
}
