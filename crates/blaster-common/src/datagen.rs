use rand::RngCore;

pub const SIZES: &[(&str, usize)] = &[
    ("1KB", 1_024),
    ("10KB", 10_240),
    ("100KB", 102_400),
    ("1MB", 1_048_576),
    ("10MB", 10_485_760),
];

pub fn parse_sizes(input: &str) -> Vec<(&'static str, usize)> {
    let requested: Vec<String> = input.split(',').map(|s| s.trim().to_lowercase()).collect();
    SIZES
        .iter()
        .filter(|(name, _)| requested.iter().any(|r| r == &name.to_lowercase()))
        .copied()
        .collect()
}

pub fn generate_payload(size: usize) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

pub fn generate_payloads(size: usize, count: usize) -> Vec<Vec<u8>> {
    (0..count).map(|_| generate_payload(size)).collect()
}
