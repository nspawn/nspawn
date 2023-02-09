use crate::modules::data_structs::InterfacesConfigs;

use {
    crate::modules::{images::Images, machines::Machines, utilities::opt_string_eval},
    comfy_table::{presets::NOTHING, *},
};

pub fn create_images_table(images: Images, width: Option<u16>) -> Table {
    let mut table = Table::new();
    table
        .load_preset(NOTHING)
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
            opt_string_eval(&image.usage),
            image.created_to_timestamp(),
            image.modified_to_timestamp(),
        ]);
    }

    table
}

pub fn create_machines_table(machines: Machines, width: Option<u16>) -> Table {
    let mut table = Table::new();
    table
        .load_preset(NOTHING)
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

pub fn create_network_configs_table(machine: &str, networks_config: &InterfacesConfigs) -> Table {
    let mut table = Table::new();

    table
        .load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Dynamic)
        // add a "Machine" header in the top to the table
        .set_header(vec![
            Cell::new("Iface Index").add_attribute(Attribute::Bold),
            Cell::new("Iface Name").add_attribute(Attribute::Bold),
            Cell::new("Flags").add_attribute(Attribute::Bold),
            Cell::new("mtu").add_attribute(Attribute::Bold),
            Cell::new("Qdisc").add_attribute(Attribute::Bold),
            Cell::new("Operstate").add_attribute(Attribute::Bold),
            Cell::new("Group").add_attribute(Attribute::Bold),
            Cell::new("txqlen").add_attribute(Attribute::Bold),
            Cell::new("Type").add_attribute(Attribute::Bold),
            Cell::new("Address").add_attribute(Attribute::Bold),
            Cell::new("Broadcast").add_attribute(Attribute::Bold),
            Cell::new("Addr Info").add_attribute(Attribute::Bold),
            Cell::new("Master").add_attribute(Attribute::Bold),
            Cell::new("Perm. Addr").add_attribute(Attribute::Bold),
            Cell::new("Index").add_attribute(Attribute::Bold),
            Cell::new("Netns id").add_attribute(Attribute::Bold),
        ]);

    // for network in networks_config {
    //     table.add_row(vec![
    //         machine.to_string(),
    //         network.ifindex.to_string(),
    //         network.ifname.to_string(),
    //         network.flags.join(","),
    //         network.mtu.to_string(),
    //         network.qdisc.to_string(),
    //         network.operstate.to_string(),
    //         network.group.to_string(),
    //         network.txqlen.unwrap_or_default().to_string(),
    //         network.link_type.to_string(),
    //         network.address.to_string(),
    //         network.broadcast.to_string(),
    //         network.master.clone().unwrap_or_default().to_string(),
    //         network.permaddr.clone().unwrap_or_default().to_string(),
    //         network.link_index.unwrap_or_default().to_string(),
    //         network.link_netnsid.unwrap_or_default().to_string(),
    //     ]);
    // }

    for network in networks_config {
        for addrinfo in &network.addr_info {
            table.add_row(vec![
                addrinfo.family.to_string(),
                addrinfo.local.to_string(),
                addrinfo.prefixlen.to_string(),
                addrinfo.scope.to_string(),
                addrinfo.label.clone().unwrap_or_default(),
                addrinfo.valid_life_time.to_string(),
                addrinfo.preferred_life_time.to_string(),
                addrinfo.metric.unwrap_or_default().to_string(),
                addrinfo.broadcast.clone().unwrap_or_default(),
                addrinfo.dynamic.unwrap_or_default().to_string(),
            ]);
        }
    }

    table
}
