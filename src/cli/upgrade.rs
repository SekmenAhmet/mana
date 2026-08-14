pub fn stub_message() -> &'static str {
    "mana upgrade: pas encore disponible en v1 (aucune release GitHub a consommer pour l'instant)"
}

pub fn run() {
    println!("{}", stub_message());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_message_explains_why_its_unavailable() {
        assert!(stub_message().contains("pas encore disponible"));
    }
}
