use lane::LaneFile;

#[test]
fn agents_can_replace_the_same_byte_range_without_overwriting_each_other() {
    let mut file = LaneFile::new(b"export const mode = 'base';\n".to_vec());

    file.write("agent-a", 21..25, b"fast".to_vec()).unwrap();
    file.write("agent-b", 21..25, b"safe".to_vec()).unwrap();

    assert_eq!(file.read_base(), b"export const mode = 'base';\n");
    assert_eq!(
        file.read("agent-a").unwrap(),
        b"export const mode = 'fast';\n"
    );
    assert_eq!(
        file.read("agent-b").unwrap(),
        b"export const mode = 'safe';\n"
    );

    file.promote("agent-a").unwrap();

    assert_eq!(file.read_base(), b"export const mode = 'fast';\n");
    assert_eq!(
        file.read("agent-b").unwrap(),
        b"export const mode = 'safe';\n"
    );
}
