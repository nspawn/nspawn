use {
    super::Images,
    crate::modules::utilities::{MACHINECTL, OUTPUT_ARGUMENTS},
    std::process::Command,
};

pub fn list_local_images() -> Result<Images, Box<dyn std::error::Error>> {
    let output = Command::new(MACHINECTL)
        .arg("list-images")
        .args(OUTPUT_ARGUMENTS)
        .output()?;

    let output = String::from_utf8(output.stdout)?;

    let images: Images = serde_json::from_str(&output)?;

    Ok(images)
}

pub fn start_image(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    Command::new(MACHINECTL).args(["start", name]).status()?;
    Ok(())
}

pub fn list_remote_images(server_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Create a reqwest client and get the list of images from the server url
    let client = reqwest::blocking::Client::new();
    let response = client.get(server_url).send()?;

    // Convert the response to a string
    let output = response.text()?;

    // Print the output
    println!("{output}");

    Ok(())
}

pub fn remove_image(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new(MACHINECTL).args(["remove", name]).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to remove image",
        )))
    }
}

pub fn rename_image(old_name: &str, new_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new(MACHINECTL)
        .args(["rename", old_name, new_name])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to rename image",
        )))
    }
}

pub fn clone_image(old_name: &str, new_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new(MACHINECTL)
        .args(["clone", old_name, new_name])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to clone image",
        )))
    }
}

pub fn import_remote_image(
    url: &str,
    image_type: &str,
    import_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::new(MACHINECTL);
    let pull_command = match image_type {
        "raw" => "pull-raw",
        "tar" => "pull-tar",
        _ => {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Invalid image type",
            )))
        }
    };

    let status = command.args([pull_command, url, import_name]).status()?;

    if status.success() {
        Ok(())
    } else {
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to import image",
        )))
    }
}

pub fn set_read_only(name: &str, value: bool) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new(MACHINECTL)
        .args(["read-only", name, &value.to_string()])
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to set read-only",
        )))
    }
}
