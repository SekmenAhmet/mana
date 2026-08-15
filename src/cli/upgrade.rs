pub(crate) fn describe_update_result(status: &self_update::Status) -> String {
    match status {
        self_update::Status::UpToDate(version) => format!("mana est deja a jour ({version})"),
        self_update::Status::Updated(version) => {
            format!("mana mis a jour vers la version {version}")
        }
    }
}

pub fn run() -> anyhow::Result<()> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner("SekmenAhmet")
        .repo_name("mana")
        .bin_name("mana")
        .target(self_update::get_target())
        .show_download_progress(true)
        .show_output(false)
        .no_confirm(true)
        .current_version(env!("CARGO_PKG_VERSION"))
        .build()?
        .update()?;
    println!("{}", describe_update_result(&status));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use self_update::Status;

    #[test]
    fn describe_update_result_reports_already_up_to_date() {
        let status = Status::UpToDate("0.1.0".to_string());
        assert_eq!(
            describe_update_result(&status),
            "mana est deja a jour (0.1.0)"
        );
    }

    #[test]
    fn describe_update_result_reports_new_version_installed() {
        let status = Status::Updated("0.2.0".to_string());
        assert_eq!(
            describe_update_result(&status),
            "mana mis a jour vers la version 0.2.0"
        );
    }
}
