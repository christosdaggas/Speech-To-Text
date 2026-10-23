const STYLE: &str = include_str!("../data/resources/style.css");

#[test]
fn swapped_surfaces_are_scoped_to_light_theme() {
    for (selector, color) in [
        (".light-theme .sidebar-box", "@sidebar_bg_color"),
        (".light-theme .content-area", "#ffffff"),
    ] {
        let rule = STYLE.split(&format!("{selector} {{")).nth(1).unwrap();
        let rule = rule.split('}').next().unwrap();
        assert!(rule.contains(&format!("background-color: {color};")));
    }
    assert!(STYLE.contains(".content-area {\n    background-color: @sidebar_bg_color;"));
    assert!(STYLE.contains(".sidebar-box {\n    background-color: @window_bg_color;"));
}

#[test]
fn sidebar_title_has_an_explicit_bold_style() {
    let rule = STYLE.split(".sidebar-app-title {").nth(1).unwrap();
    assert!(rule
        .split('}')
        .next()
        .unwrap()
        .contains("font-weight: 700;"));
}
