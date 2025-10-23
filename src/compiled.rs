pub fn is_bytecode(data: &[u8]) -> bool {
    if data.len() < 5 {
        return false;
    }

    let header = &data[0..4];
    let first_byte = data[0];

    header == [0x1b, b'L', b'u', b'a']
        || header == [0x1b, b'L', b'J', 0x1]
        || header == [0x1b, b'L', b'J', 0x2]
        || matches!(first_byte, 3..=6)
}

pub fn get_bytecode_from_file(
    filename: &str,
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    use base64::{engine::general_purpose, Engine as _};
    use std::fs;

    let file_contents = fs::read(filename)?;

    // check for direct bytecode
    if is_bytecode(&file_contents) {
        let bytecode = general_purpose::STANDARD.encode(&file_contents);
        return Ok((bytecode, None));
    }

    // try decoding as base64
    if let Ok(decoded) = general_purpose::STANDARD.decode(&file_contents) {
        if is_bytecode(&decoded) {
            let bytecode = String::from_utf8_lossy(&file_contents).to_string();
            return Ok((bytecode, None));
        }
    }

    // try extracting from rbxlx-style header
    let file_string = String::from_utf8_lossy(&file_contents);
    
    let bytecode_start_lf = "-- Bytecode (Base64):\n-- ";
    let bytecode_start_crlf = "-- Bytecode (Base64):\r\n-- ";

    let (position, bytecode_start_len) = file_string
        .find(bytecode_start_lf)
        .map(|pos| (pos, bytecode_start_lf.len()))
        .or_else(|| file_string
            .find(bytecode_start_crlf)
            .map(|pos| (pos, bytecode_start_crlf.len())))
        .unzip();

    if let (Some(position), Some(bytecode_start_len)) = (position, bytecode_start_len) {
        let start_pos = position + bytecode_start_len;

        let bytecode_end = file_string[start_pos..]
            .find(|c| c == '\n' || c == '\r')
            .map(|idx| start_pos + idx)
            .unwrap_or(file_string.len());

        let header = file_string[..start_pos].to_string();
        let bytecode = &file_string[start_pos..bytecode_end];

        return Ok((bytecode.to_string(), Some(header)));
    }

    Err("no bytecode found in file".into())
}
