use crate::modules::data_structs::InterfacesConfigs;

use {
    super::Machines,
    crate::modules::utilities::{BIN_PATH, MACHINECTL, OUTPUT_ARGUMENTS},
    std::process::Command,
};

pub fn list_active_machines() -> Result<Machines, Box<dyn std::error::Error>> {
    let output = Command::new(MACHINECTL).args(OUTPUT_ARGUMENTS).output()?;

    let output = String::from_utf8(output.stdout)?;

    let machines: Machines = serde_json::from_str(&output)?;

    Ok(machines)
}

pub fn stop_machine(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    Command::new(MACHINECTL).args(["stop", name]).status()?;
    Ok(())
}

pub fn terminate_machine(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    Command::new(MACHINECTL)
        .args(["terminate", name])
        .status()?;
    Ok(())
}

pub fn kill_machine(name: &str, signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    Command::new(MACHINECTL)
        .args(["kill", name, "--signal", signal])
        .status()?;
    Ok(())
}

pub fn reboot_machine(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    Command::new(MACHINECTL).args(["reboot", name]).status()?;
    Ok(())
}

pub fn machine_shell(name: &str, user: &str) -> Result<(), Box<dyn std::error::Error>> {
    let login = format!("{user}@{name}");
    Command::new(MACHINECTL)
        .args(["--quiet", "shell", &login])
        .status()?;
    Ok(())
}

pub fn exec_machine(
    name: &str,
    user: &str,
    command: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let command_vec = command.split_whitespace().collect::<Vec<&str>>();

    let command = format!("{}{}", BIN_PATH, command_vec[0]);
    let login = format!("{user}@{name}");

    Command::new(MACHINECTL)
        .args(["--quiet", "shell", &login, &command])
        .args(&command_vec[1..])
        .status()?;
    Ok(())
}

pub fn get_network_config(name: &str) -> Result<InterfacesConfigs, Box<dyn std::error::Error>> {
    let configs = String::from_utf8(
        Command::new(MACHINECTL)
            .args(["--quiet", "shell", name, "/usr/bin/ip", "-j", "a"])
            .output()?
            .stdout,
    )?;

    let configs: InterfacesConfigs = serde_json::from_str(&configs)?;

    Ok(configs)
}
