use crate::{
    error_msg,
    modules::{
        args::{
            arguments::{Arguments, Commands},
            images_args::{ImageCommands, ImagesArgs},
            machines_args::{MachineCommands, MachinesArgs},
            RemoteOps,
        },
        data_format::{create_images_table, create_machines_table, create_network_configs_table},
        images::{
            import_remote_image, list_local_images, list_remote_images, remove_image, rename_image,
            set_read_only, start_image, Images,
        },
        machines::{
            exec_machine, get_network_config, kill_machine, list_active_machines, machine_shell,
            reboot_machine, stop_machine, terminate_machine, Machines,
        },
        utilities::{invalid_image, invalid_machine, opt_string_eval},
    },
    success_msg, warn_msg,
};

pub fn handle_arguments(arguments: Arguments) -> Result<(), Box<dyn std::error::Error>> {
    match arguments.cmd {
        Some(Commands::Images(images_args)) => manage_images(images_args, arguments.width)?,
        Some(Commands::Machines(machines_args)) => manage_machines(machines_args, arguments.width)?,
        None => (),
    };

    Ok(())
}

pub fn manage_images(
    imgs: ImagesArgs,
    width: Option<u16>,
) -> Result<(), Box<dyn std::error::Error>> {
    match imgs.commands {
        ImageCommands::List(list) => {
            let images: Images = if list.ro {
                list_local_images()?
                    .into_iter()
                    .filter(|image| image.ro)
                    .collect()
            } else if let Some(type_field) = list.type_field {
                list_local_images()?
                    .into_iter()
                    .filter(|image| image.type_field == type_field)
                    .collect()
            } else if let Some(pattern) = list.pattern {
                list_local_images()?
                    .into_iter()
                    .filter(|image| image.name.to_lowercase().contains(&pattern.to_lowercase()))
                    .collect()
            } else {
                list_local_images()?
            };

            println!("{}", create_images_table(images, width));
        }
        ImageCommands::Start(start) => {
            if let Some(machine) = start.machine {
                start_image(&machine)?;
            } else if start.all {
                warn_msg!("Warning, starting all images...");
                list_local_images()?.into_iter().for_each(|machine| {
                    if let Err(e) = start_image(&machine.name) {
                        error_msg!("Error starting image {}: {}", machine.name, e);
                    } else {
                        success_msg!("Started image: {}", machine.name);
                    }
                });
            } else if let Some(pattern) = start.pattern {
                warn_msg!("Starting images matching pattern: {}", pattern);
                list_local_images()?.into_iter().for_each(|machine| {
                    if machine
                        .name
                        .to_lowercase()
                        .contains(&pattern.to_lowercase())
                    {
                        if let Err(e) = start_image(&machine.name) {
                            error_msg!("Error starting image {}: {}", machine.name, e);
                        } else {
                            success_msg!("Started image: {}", machine.name);
                        }
                    }
                });
            } else {
                invalid_image();
            }
        }
        ImageCommands::Remove(remove) => {
            if let Some(image) = remove.image {
                warn_msg!("Removing image {}...", image);
                if let Err(e) = remove_image(&image) {
                    error_msg!("Failed to remove image {}: {}!", image, e);
                } else {
                    success_msg!("Image {} removed!", image);
                }
            } else if remove.all {
                warn_msg!("Removing all images...");
                list_local_images()?.into_iter().for_each(|image| {
                    if image.ro {
                        warn_msg!("Skipping read-only image: {}", image.name);
                    } else if let Err(e) = remove_image(&image.name) {
                        error_msg!("Error removing image {}: {}", image.name, e);
                    } else {
                        success_msg!("Image {} removed.", image.name);
                    }
                });
            } else if let Some(pattern) = remove.pattern {
                warn_msg!("Removing images matching pattern: {}", pattern);
                list_local_images()?.into_iter().for_each(|image| {
                    if image.ro {
                        warn_msg!("Skipping read-only image: {}", image.name);
                    } else if image.name.to_lowercase().contains(&pattern.to_lowercase()) {
                        if let Err(e) = remove_image(&image.name) {
                            error_msg!("Error removing image {}: {}", image.name, e);
                        } else {
                            success_msg!("Image {} removed.", image.name);
                        }
                    }
                });
            } else {
                invalid_image();
            }
        }
        ImageCommands::Set(set) => {
            if set.read_only {
                warn_msg!("Setting image {} to read-only...", set.image);
                if let Err(e) = set_read_only(&set.image, true) {
                    error_msg!("Error setting image {} to read-only: {}", set.image, e);
                } else {
                    success_msg!("Image {} set to read-only.", set.image);
                }
            } else if set.read_write {
                warn_msg!("Setting image {} to read-write...", set.image);
                if let Err(e) = set_read_only(&set.image, false) {
                    error_msg!("Error setting image {} to read-write: {}", set.image, e);
                } else {
                    success_msg!("Image {} set to read-write.", set.image);
                }
            } else {
                error_msg!("No action specified! See --help for more information.");
            }
        }
        ImageCommands::Clone(clone) => {
            warn_msg!("Cloning image {}...", clone.current_image);
            if let Err(e) =
                crate::modules::images::clone_image(&clone.current_image, &clone.new_image)
            {
                error_msg!("Error cloning image {}: {}", clone.current_image, e);
            } else {
                success_msg!(
                    "Image {} sucessfully cloned to {}.",
                    clone.current_image,
                    clone.new_image
                );
            }
        }
        ImageCommands::Rename(rename) => {
            warn_msg!(
                "Renaming image {} to {}...",
                rename.current_image,
                rename.new_image
            );
            if let Err(e) = rename_image(&rename.current_image, &rename.new_image) {
                error_msg!("Error renaming image {}: {}", rename.current_image, e);
            } else {
                success_msg!(
                    "Image {} sucessfully renamed to {}.",
                    rename.current_image,
                    rename.new_image
                );
            }
        }

        ImageCommands::Remote(remote) => {
            match remote.remote_ops {
                RemoteOps::List(list) => {
                    // Lets use remote.server.unwrap() here because the server is always Some()
                    // Lets use remote.server.unwrap() here because the server is always Some()
                    // TODO: add pattern support
                    let listing_url = list.server.unwrap() + "/storage/list.txt";
                    if let Some(_pattern) = list.pattern {
                        list_remote_images(&listing_url)?;
                    } else {
                        list_remote_images(&listing_url)?;
                    }
                }
                RemoteOps::Import(import) => {
                    let local_name = if let Some(local_name) = import.local_name {
                        local_name
                    } else {
                        import.remote_name.replace('/', "-")
                    };

                    warn_msg!("Importing image {} to {local_name}...", import.remote_name);

                    // Get the image type, it's the last part of the remote name split by /
                    let image_type = import
                        .remote_name
                        .split('/')
                        .last()
                        .expect("Failed to get image type");

                    let remote_url = import.server.unwrap()
                        + "/storage/"
                        + &import.remote_name
                        + "/"
                        + "image."
                        + image_type
                        + ".xz";

                    if let Err(e) = import_remote_image(&remote_url, image_type, &local_name) {
                        error_msg!(
                            "Error importing image {} from url {}: {}",
                            import.remote_name,
                            remote_url,
                            e
                        );
                    } else {
                        success_msg!("Image {} imported!", &local_name);
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn manage_machines(
    machines: MachinesArgs,
    width: Option<u16>,
) -> Result<(), Box<dyn std::error::Error>> {
    match machines.commands {
        MachineCommands::Stop(stop) => {
            if let Some(machine) = stop.machine {
                stop_machine(&machine)?;
            } else if stop.all {
                warn_msg!("Warning, stopping all machines...");
                list_active_machines()?.into_iter().for_each(|machine| {
                    let machine = opt_string_eval(&machine.machine);
                    if let Err(e) = stop_machine(&machine) {
                        error_msg!("Error stopping machine {}: {}", &machine, e);
                    } else {
                        success_msg!("Stopped machine: {}", &machine);
                    }
                });
            } else if let Some(pattern) = stop.pattern {
                warn_msg!("Stopping machines matching pattern: {}", pattern);
                list_active_machines()?.into_iter().for_each(|machine| {
                    let machine = opt_string_eval(&machine.machine);
                    if machine.to_lowercase().contains(&pattern.to_lowercase()) {
                        if let Err(e) = stop_machine(&machine) {
                            error_msg!("Error starting machine {}: {}", machine, e);
                        } else {
                            success_msg!("Stopped machine: {}", machine);
                        }
                    }
                });
            } else {
                invalid_machine();
            }
        }
        MachineCommands::Kill(kill) => {
            if let Some(machine) = kill.machine {
                kill_machine(&machine, &kill.signal)?;
            } else if kill.all {
                warn_msg!("Warning, killing all machines...");
                list_active_machines()?.into_iter().for_each(|machine| {
                    let machine = opt_string_eval(&machine.machine);
                    if let Err(e) = kill_machine(&machine, &kill.signal) {
                        error_msg!("Error killing machine {}: {}", &machine, e);
                    }
                });
            } else if let Some(pattern) = kill.pattern {
                warn_msg!("Killing machines matching pattern: {}", pattern);
                list_active_machines()?.into_iter().for_each(|machine| {
                    let machine = opt_string_eval(&machine.machine);
                    if machine.to_lowercase().contains(&pattern.to_lowercase()) {
                        if let Err(e) = kill_machine(&machine, &kill.signal) {
                            error_msg!("Error killing machine {}: {}", machine, e);
                        }
                    }
                });
            } else {
                invalid_machine();
            }
        }
        MachineCommands::List(list) => {
            let machines: Machines = if let Some(pattern) = list.pattern {
                list_active_machines()?
                    .into_iter()
                    .filter(|machine| {
                        opt_string_eval(&machine.machine)
                            .to_lowercase()
                            .contains(&pattern.to_lowercase())
                    })
                    .collect()
            } else {
                list_active_machines()?.into_iter().collect()
            };

            println!("{}", create_machines_table(machines, width));
        }
        MachineCommands::Exec(exec) => {
            let user = exec.user.unwrap_or_else(|| String::from("root"));

            if let Some(machine) = exec.machine {
                exec_machine(&machine, &user, &exec.command)?;
            } else if exec.all {
                warn_msg!("Warning, executing command on all machines...");
                list_active_machines()?.into_iter().for_each(|machine| {
                    let machine = opt_string_eval(&machine.machine);
                    if let Err(e) = exec_machine(&machine, &user, &exec.command) {
                        error_msg!("Error executing command on machine {}: {}", &machine, e);
                    } else {
                        success_msg!(
                            "Executed command {} on machine: {}",
                            &exec.command,
                            &machine
                        );
                    }
                });
            } else if let Some(pattern) = exec.pattern {
                warn_msg!(
                    "Executing command on machines matching pattern: {}",
                    pattern
                );
                list_active_machines()?.into_iter().for_each(|machine| {
                    let machine = opt_string_eval(&machine.machine);
                    if machine.to_lowercase().contains(&pattern.to_lowercase()) {
                        success_msg!("Executing command on machine: {}", machine);
                        if let Err(e) = exec_machine(&machine, &user, &exec.command) {
                            error_msg!("Error executing command on machine {}: {}", machine, e);
                        } else {
                            success_msg!(
                                "Executed command {} on machine: {}",
                                &exec.command,
                                &machine
                            );
                        }
                    }
                });
            } else {
                invalid_machine();
            }
        }
        MachineCommands::Shell(shell) => {
            let user = shell.user.unwrap_or_else(|| String::from("root"));

            if let Some(machine) = shell.machine {
                machine_shell(&machine, &user)?;
            } else {
                invalid_machine();
            }
        }
        MachineCommands::Terminate(terminate) => {
            if let Some(machine) = terminate.machine {
                terminate_machine(&machine)?;
            } else if terminate.all {
                warn_msg!("Warning, terminating all machines...");
                list_active_machines()?.into_iter().for_each(|machine| {
                    let machine = opt_string_eval(&machine.machine);
                    if let Err(e) = terminate_machine(&machine) {
                        error_msg!("Error terminating machine {}: {}", &machine, e);
                    } else {
                        success_msg!("Terminated machine: {}", &machine);
                    }
                });
            } else if let Some(pattern) = terminate.pattern {
                warn_msg!("Terminating machines matching pattern: {}", pattern);
                list_active_machines()?.into_iter().for_each(|machine| {
                    let machine = opt_string_eval(&machine.machine);
                    if machine.to_lowercase().contains(&pattern.to_lowercase()) {
                        if let Err(e) = terminate_machine(&machine) {
                            error_msg!("Error terminating machine {}: {}", machine, e);
                        } else {
                            success_msg!("Terminated machine: {}", machine);
                        }
                    }
                });
            } else {
                invalid_machine();
            }
        }
        MachineCommands::Reboot(reboot) => {
            if let Some(machine) = reboot.machine {
                reboot_machine(&machine)?;
            } else if reboot.all {
                warn_msg!("Warning, rebooting all machines...");
                list_active_machines()?.into_iter().for_each(|machine| {
                    let machine = opt_string_eval(&machine.machine);
                    if let Err(e) = reboot_machine(&machine) {
                        error_msg!("Error rebooting machine {}: {}", machine, e);
                    } else {
                        success_msg!("Rebooted machine: {}", machine);
                    }
                });
            } else if let Some(pattern) = reboot.pattern {
                warn_msg!("Rebooting machines matching pattern: {}", pattern);
                list_active_machines()?.into_iter().for_each(|machine| {
                    let machine = opt_string_eval(&machine.machine);
                    if machine.to_lowercase().contains(&pattern.to_lowercase()) {
                        if let Err(e) = reboot_machine(&machine) {
                            error_msg!("Error rebooting machine {}: {}", machine, e);
                        } else {
                            success_msg!("Rebooted machine: {}", machine);
                        }
                    }
                });
            } else {
                invalid_machine();
            }
        }
        MachineCommands::Network(network) => {
            if let Some(machine) = network.machine {
                println!(
                    "{}",
                    create_network_configs_table(&machine, &get_network_config(&machine)?)
                );
            } else if network.all {
                warn_msg!("Warning, getting network info for all machines...");
                list_active_machines()?.into_iter().for_each(|machine| {
                    let machine = opt_string_eval(&machine.machine);
                    if let Ok(ifaces_config) = get_network_config(&machine) {
                        println!("{}", create_network_configs_table(&machine, &ifaces_config));
                    } else {
                        error_msg!("Error getting network info for machine: {}", machine);
                    }
                });
            } else if let Some(pattern) = network.pattern {
                warn_msg!(
                    "Getting network info for machines matching pattern: {}",
                    pattern
                );
                list_active_machines()?.into_iter().for_each(|machine| {
                    let machine = opt_string_eval(&machine.machine);
                    if machine.to_lowercase().contains(&pattern.to_lowercase()) {
                        if let Ok(ifaces_config) = get_network_config(&machine) {
                            println!("{}", create_network_configs_table(&machine, &ifaces_config));
                        } else {
                            error_msg!("Error getting network info for machine: {}", machine);
                        }
                    }
                });
            } else {
                invalid_machine();
            }
        }
    }

    Ok(())
}
