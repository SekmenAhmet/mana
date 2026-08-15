#[allow(dead_code)]
pub(crate) fn describe_update_result(status: &self_update::Status) -> String {
    match status {
        self_update::Status::UpToDate(version) => format!("mana est deja a jour ({version})"),
        self_update::Status::Updated(version) => {
            format!("mana mis a jour vers la version {version}")
        }
    }
}

pub fn run() -> anyhow::Result<()> {
    todo!()
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
