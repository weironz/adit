use super::*;

fn term(cols: u16, rows: u16) -> VtTerminal {
    VtTerminal::new(TerminalSize::new(cols, rows))
}

fn text(t: &VtTerminal) -> String {
    t.snapshot(Viewport::visible(100))
        .lines
        .iter()
        .flat_map(|line| line.cells.iter())
        .map(|cell| cell.text.as_str())
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn images(t: &VtTerminal) -> Vec<crate::TerminalImage> {
    t.snapshot(Viewport::visible(100))
        .lines
        .into_iter()
        .flat_map(|line| line.images)
        .collect()
}

const RED_TILE: &[u8] = b"\x1bP0;1q\"1;1;8;6#1;2;100;0;0!8~\x1b\\";

#[test]
fn combining_marks_are_streamed_with_their_base_and_pen() {
    let mut t = term(12, 3);
    for byte in "e\u{301}\u{754c}\u{308}x".as_bytes() {
        t.feed(&[*byte]);
    }
    assert_eq!(text(&t), "e\u{301}\u{754c}\u{308}x");
    assert_eq!(t.state.cursor_col, 4);
    assert_eq!(t.state.grid[0][0].text(), "e\u{301}");
    assert_eq!(t.state.grid[0][1].text(), "\u{754c}\u{308}");
    assert!(t.state.grid[0][2].spacer);
    t.feed_str("\x1b[31m\u{302}");
    assert_eq!(t.state.grid[0][3].text(), "x\u{302}");
    assert_eq!(t.state.grid[0][3].pen.fg, Color::Default);
}

#[test]
fn margin_marks_do_not_wrap_and_overwrite_drops_the_whole_cluster() {
    let mut t = term(2, 2);
    t.feed_str("ab\u{301}");
    assert_eq!(t.state.cursor_row, 0);
    assert!(t.state.pending_wrap);
    assert_eq!(t.state.grid[0][1].text(), "b\u{301}");
    t.feed_str("\rx");
    assert_eq!(text(&t), "xb\u{301}");
    t.feed_str("y");
    assert_eq!(text(&t), "xy");
}

#[test]
fn leading_marks_and_cluster_limit_are_bounded() {
    let mut t = term(4, 2);
    t.feed_str("\u{301}");
    assert_eq!(t.state.cursor_col, 0);
    assert_eq!(t.state.grid[0][0].text(), " \u{301}");
    t.feed_str("x");
    t.feed_str(&"\u{301}".repeat(10_000));
    assert!(t.state.grid[0][0].text().len() <= MAX_GLYPH_BYTES);
    assert_eq!(t.state.cursor_col, 1);
    t.feed_str("\rx");
    assert_eq!(t.state.grid[0][0].text(), "x");
}

#[test]
fn emoji_joiners_modifiers_and_variation_selectors_share_cells() {
    for (input, width) in [
        ("\u{1f469}\u{200d}\u{1f4bb}", 2),
        ("\u{1f44d}\u{1f3fd}", 2),
        ("\u{2764}\u{fe0f}", 2),
    ] {
        let mut t = term(8, 2);
        for byte in input.as_bytes() {
            t.feed(&[*byte]);
        }
        assert_eq!(t.state.grid[0][0].text(), input);
        assert_eq!(t.state.cursor_col, width);
        assert_eq!(crate::glyphs(input).collect::<Vec<_>>(), vec![input]);
        assert_eq!(crate::glyph_width(input), width);
    }
}

#[test]
fn extended_graphemes_preserve_flags_and_indic_clusters_across_packets() {
    for input in ["🇺🇸", "🇬🇧", "क्ष"] {
        let mut terminal = term(12, 2);
        for byte in input.as_bytes() {
            terminal.feed(&[*byte]);
        }
        assert_eq!(terminal.state.grid[0][0].text(), input);
        assert_eq!(terminal.state.cursor_col, crate::glyph_width(input));
        assert_eq!(crate::glyphs(input).collect::<Vec<_>>(), vec![input]);
    }
    let text = "x\u{200b}y";
    assert_eq!(crate::glyphs(text).collect::<Vec<_>>(), vec!["x\u{200b}", "y"]);
}

#[test]
fn combining_clusters_survive_reflow_and_wide_half_replacement() {
    let mut t = term(8, 3);
    t.feed_str("a\u{301}\u{754c}\u{308}bcde");
    let before = text(&t);
    let anchor = t.logical_anchor(0, 3);
    t.resize(TerminalSize::new(3, 3));
    assert_eq!(text(&t), before);
    let (row, col) = t.resolve_anchor(anchor);
    assert_eq!(t.state.row_at(row)[col].ch, 'b');
    t.resize(TerminalSize::new(8, 3));
    assert_eq!(text(&t), before);

    let mut t = term(8, 2);
    t.feed_str("\u{754c}\u{308}\x1b[1;2HX");
    assert_eq!(text(&t), " X");
    assert!(t.state.grid[0][0].glyph.is_none());
    assert!(!t.state.grid[0][1].spacer);
}

#[test]
fn wide_insert_delete_and_erase_keep_grid_ownership_valid() {
    let mut t = term(8, 2);
    t.feed_str("a\u{754c}\u{301}b\x1b[1;2H\x1b[@");
    assert_eq!(text(&t), "a \u{754c}\u{301}b");
    t.feed_str("\x1b[P");
    assert_eq!(text(&t), "a\u{754c}\u{301}b");
    t.feed_str("\x1b[1;3H\x1b[X");
    assert_eq!(text(&t), "a  b");
    for (col, cell) in t.state.grid[0].iter().enumerate() {
        if cell.spacer {
            assert!(col > 0 && t.state.grid[0][col - 1].width() == 2);
        }
    }
}

#[test]
fn one_column_terminal_never_loses_wide_clusters_or_panics() {
    let mut t = term(1, 2);
    t.feed_str("\u{754c}\u{301}");
    assert_eq!(t.state.cursor_row, 0);
    assert_eq!(text(&t), "\u{754c}\u{301}");
    t.resize(TerminalSize::new(4, 2));
    assert_eq!(text(&t), "\u{754c}\u{301}");
}

#[test]
fn dec_designation_shift_save_restore_and_unicode_are_independent() {
    let mut t = term(30, 2);
    t.feed_str("\x1b)0\x0elqk\x0f#\x1b(A#\x1b(B\u{754c}");
    assert_eq!(text(&t), "\u{250c}\u{2500}\u{2510}#\u{a3}\u{754c}");
    t.feed_str("\x1b*0\x1bNqz");
    assert!(text(&t).ends_with("\u{2500}z"));
    t.feed_str("\x1b(0\x1b7\x1b(Bq\x1b8x");
    assert!(text(&t).ends_with("\u{2502}"));
    t.feed_str("\x1bc#q");
    assert_eq!(text(&t), "#q");
}

#[test]
fn alternate_screen_restores_designated_character_sets() {
    let mut t = term(8, 2);
    t.feed_str("\x1b(0q\x1b[?1049h\x1b(Balt\x1b[?1049lq");
    assert_eq!(text(&t), "\u{2500}\u{2500}");
}

#[test]
fn custom_tabs_forward_backward_clear_and_reset() {
    let mut t = term(25, 2);
    t.feed_str("\t");
    assert_eq!(t.state.cursor_col, 8);
    t.feed_str("\x1b[3g\x1b[4G\x1bH\x1b[12G\x1bH\r\t");
    assert_eq!(t.state.cursor_col, 3);
    t.feed_str("\x1b[I");
    assert_eq!(t.state.cursor_col, 11);
    t.feed_str("\x1b[Z");
    assert_eq!(t.state.cursor_col, 3);
    t.feed_str("\x1b[g\r\t");
    assert_eq!(t.state.cursor_col, 11);
    t.feed_str("\x1b[65535I");
    assert_eq!(t.state.cursor_col, 24);
    t.feed_str("\x1b[65535Z");
    assert_eq!(t.state.cursor_col, 0);
    t.feed_str("\x1bc\t");
    assert_eq!(t.state.cursor_col, 8);
}

#[test]
fn tab_stops_survive_resize_and_new_columns_get_defaults() {
    let mut t = term(10, 2);
    t.feed_str("\x1b[3g\x1b[4G\x1bH");
    t.resize(TerminalSize::new(20, 2));
    t.feed_str("\r\t");
    assert_eq!(t.state.cursor_col, 3);
    t.feed_str("\t");
    assert_eq!(t.state.cursor_col, 16);
    t.resize(TerminalSize::new(5, 2));
    t.resize(TerminalSize::new(20, 2));
    t.feed_str("\r\t\t");
    assert_eq!(t.state.cursor_col, 8);
}

#[test]
fn changes_track_writes_cursors_and_silent_sequences() {
    let mut t = term(10, 4);
    assert_eq!(t.feed(b"x").dirty_rows, vec![0]);
    assert!(t.feed(b"").dirty_rows.is_empty());
    assert!(t.feed(b"\x1b[31m").dirty_rows.is_empty());
    assert!(t.feed(b"\x1b[6n").dirty_rows.is_empty());
    assert!(t.feed(b"\x1b]2;title\x07").dirty_rows.is_empty());
    assert!(t.feed(b"\x07").dirty_rows.is_empty());
    assert_eq!(t.feed(b"\x1b[3;1H").dirty_rows, vec![0, 2]);
    assert_eq!(t.feed(b"\x1b[?25l").dirty_rows, vec![2]);
    assert!(t.feed(b"\x1b[2;1H").dirty_rows.is_empty());
    assert_eq!(t.feed(b"\x1b[?25h").dirty_rows, vec![1]);
    assert_eq!(t.feed(b"\x1b[2K").dirty_rows, vec![1]);
    assert!(t.resize(TerminalSize::new(10, 4)).dirty_rows.is_empty());
    assert_eq!(t.resize(TerminalSize::new(9, 3)).dirty_rows, vec![0, 1, 2]);
}

#[test]
fn changes_track_scrolling_screen_swap_and_fragmented_controls() {
    let mut t = term(10, 5);
    t.feed_str("\x1b[?25l\x1b[2;4r");
    assert_eq!(t.feed(b"\x1b[S").dirty_rows, vec![1, 2, 3]);
    assert_eq!(t.feed(b"\x1b[T").dirty_rows, vec![1, 2, 3]);
    assert_eq!(t.feed(b"\x1b[?1049h").dirty_rows, vec![0, 1, 2, 3, 4]);
    assert_eq!(t.feed(b"\x1b[?1049l").dirty_rows, vec![0, 1, 2, 3, 4]);
    assert!(t.feed(b"\x1b[").dirty_rows.is_empty());
    assert_eq!(t.feed(b"2K").dirty_rows, vec![0]);
    assert_eq!(t.feed(b"\x1bc").dirty_rows, vec![0, 1, 2, 3, 4]);
}

#[test]
fn changes_mark_combining_suffixes_and_graphics_only_when_published() {
    let mut t = term(10, 3);
    t.feed_str("e");
    assert_eq!(t.feed("\u{301}".as_bytes()).dirty_rows, vec![0]);
    assert!(t
        .feed(&RED_TILE[..RED_TILE.len() - 1])
        .dirty_rows
        .is_empty());
    assert_eq!(t.feed(b"\\").dirty_rows, vec![0, 1]);
}

#[test]
fn sixel_reaches_render_snapshot_on_every_packet_boundary() {
    let mut expected = term(12, 4);
    expected.feed(RED_TILE);
    let snapshot = expected.snapshot(Viewport::visible(4));
    assert_eq!(snapshot.lines[0].images.len(), 1);
    assert_eq!(&snapshot.lines[0].images[0].rgba[..4], &[255, 0, 0, 255]);
    assert_eq!(snapshot.cursor_row, 1);
    for split in 0..=RED_TILE.len() {
        let mut t = term(12, 4);
        t.feed(&RED_TILE[..split]);
        t.feed(&RED_TILE[split..]);
        assert_eq!(t.snapshot(Viewport::visible(4)), snapshot, "split {split}");
    }
}

#[test]
fn cancelled_unknown_and_oversized_dcs_never_publish() {
    let mut t = term(12, 3);
    for ending in [b"\x18".as_slice(), b"\x1a", b"\x1b[0m"] {
        t.feed(&RED_TILE[..RED_TILE.len() - 2]);
        t.feed(ending);
        assert!(images(&t).is_empty());
    }
    t.feed(b"\x1bP+qunknown\x1b\\");
    assert!(images(&t).is_empty());
    assert_eq!(text(&t), "");
    t.feed(b"\x1bPq");
    t.feed(&vec![b'?'; crate::sixel::MAX_BYTES + 1]);
    t.feed(b"\x1b\\OK");
    assert!(images(&t).is_empty());
    assert_eq!(text(&t), "OK");
}

#[test]
fn c1_st_terminates_sixel_and_following_text_is_parsed_normally() {
    let mut t = term(8, 3);
    t.feed(&RED_TILE[..RED_TILE.len() - 2]);
    t.feed(b"\x9cOK");
    assert_eq!(images(&t).len(), 1);
    assert!(text(&t).ends_with("OK"));
}

#[test]
fn sixel_scrolls_into_history_and_clear_overwrite_and_alt_screen_work() {
    let mut t = term(8, 2);
    t.feed(RED_TILE);
    t.feed_str("\r\n");
    assert_eq!(
        t.state.scrollback[0][0].image.as_ref().unwrap().len(),
        8 * 16 * 4
    );
    assert_eq!(images(&t).len(), 1);
    t.feed_str("\x1b[?1049h");
    assert!(t
        .snapshot(Viewport::tail(2))
        .lines
        .iter()
        .all(|line| line.images.is_empty()));
    t.feed_str("\x1b[?1049l");
    assert_eq!(images(&t).len(), 1);
    t.clear();
    assert!(images(&t).is_empty());

    let mut t = term(8, 3);
    t.feed(RED_TILE);
    t.feed_str("\x1b[HX");
    assert!(images(&t).is_empty());
}

#[test]
fn sixel_tiles_reflow_with_cells_and_survive_alt_resize() {
    let mut t = term(8, 3);
    t.feed_str("ab");
    t.feed(RED_TILE);
    let pixels = images(&t)[0].rgba.clone();
    assert_eq!(images(&t)[0].col, 2);
    t.resize(TerminalSize::new(2, 3));
    assert_eq!(images(&t)[0].col, 0);
    assert_eq!(images(&t)[0].rgba, pixels);
    t.resize(TerminalSize::new(8, 3));
    assert_eq!(images(&t)[0].col, 2);
    t.feed_str("\x1b[?1049h");
    t.resize(TerminalSize::new(4, 3));
    t.feed_str("\x1b[?1049l");
    assert_eq!(images(&t)[0].rgba, pixels);
}

#[test]
fn sixel_display_mode_clips_without_moving_text_cursor_and_preserves_transparency() {
    let mut t = term(2, 2);
    t.feed_str("\x1b[?80l\x1b[2;2H");
    t.feed(RED_TILE);
    assert_eq!((t.state.cursor_row, t.state.cursor_col), (1, 1));
    t.feed(b"\x1bP0;1q#1;2;0;100;0@\x1b\\");
    let image = images(&t).remove(0);
    assert_eq!(&image.rgba[..8], &[0, 255, 0, 255, 255, 0, 0, 255]);
    t.feed(b"\x1bP0;1q\"1;1;24;6!24~\x1b\\");
    assert_eq!(images(&t).len(), 2);
}

#[test]
fn retained_graphics_have_a_terminal_wide_budget() {
    let mut t = term(128, 2);
    let pixel = Arc::new(vec![0; 8 * 16 * 4]);
    for _ in 0..MAX_GRAPHIC_TILES / 128 {
        let mut row = Row::blank(128, Color::Default);
        for cell in &mut row.cells {
            cell.image = Some(pixel.clone());
        }
        t.state.scrollback.push_back(row);
    }
    t.feed(RED_TILE);
    let count: usize = t
        .state
        .scrollback
        .iter()
        .chain(t.state.grid.iter())
        .map(|row| row.cells.iter().filter(|cell| cell.image.is_some()).count())
        .sum();
    assert_eq!(count, MAX_GRAPHIC_TILES);
    assert!(t.state.scrollback[0][0].image.is_none());
}

#[test]
fn dcs_status_reports_real_sgr_and_scroll_region_without_injection() {
    let mut t = term(10, 5);
    t.feed_str("\x1b[1;38;2;1;2;3m\x1b[2;4r\x1bP$qm\x1b\\\x1bP$qr\x1b\\");
    assert_eq!(
        t.take_responses(),
        b"\x1bP1$r0;1;38;2;1;2;3m\x1b\\\x1bP1$r2;4r\x1b\\"
    );
    t.feed_str("\x1bP$qunknown\x1b\\");
    assert_eq!(t.take_responses(), b"\x1bP0$r\x1b\\");
    t.feed_str("\x1bP$qm\x18");
    assert!(t.take_responses().is_empty());
}

#[test]
fn graphics_capability_and_geometry_reports_follow_resize() {
    let mut t = term(80, 24);
    assert!(t
        .feed(b"\x1b[c\x1b[14t\x1b[16t\x1b[18t")
        .dirty_rows
        .is_empty());
    assert_eq!(
        t.take_responses(),
        b"\x1b[?62;4;22c\x1b[4;384;640t\x1b[6;16;8t\x1b[8;24;80t"
    );
    t.resize(TerminalSize::new(40, 12));
    t.feed(b"\x1b[14t");
    assert_eq!(t.take_responses(), b"\x1b[4;192;320t");
}

#[test]
fn opaque_sixel_uses_the_terminal_background_and_clears_old_tiles() {
    let mut t = term(8, 3);
    t.feed(RED_TILE);
    t.feed_str("\x1b[H\x1b[44m");
    t.feed(b"\x1bP7;0q?\x1b\\");
    let image = &t.state.grid[0][0];
    assert_eq!(image.pen.bg, Color::Indexed(4));
    assert_eq!(image.ch, ' ');
    assert!(image.image.as_ref().unwrap().iter().all(|byte| *byte == 0));
}

#[test]
fn graphics_on_a_wide_spacer_survive_narrow_reflow() {
    let mut t = term(8, 3);
    t.feed_str("\u{754c}\u{301}\x1b[1;2H");
    t.feed(RED_TILE);
    let pixels = images(&t)[0].rgba.clone();
    assert_eq!(images(&t)[0].col, 1);
    t.resize(TerminalSize::new(1, 3));
    t.resize(TerminalSize::new(8, 3));
    assert_eq!(images(&t)[0].col, 1);
    assert_eq!(images(&t)[0].rgba, pixels);
    assert!(text(&t).starts_with("\u{754c}\u{301}"));
}

#[test]
fn replacement_and_erase_damage_only_the_affected_row() {
    let mut t = term(12, 4);
    t.feed_str("\x1b[3;1H\u{754c}\u{301}ab");
    assert_eq!(t.feed(b"\rX").dirty_rows, vec![2]);
    assert!(t.state.grid[2][1].glyph.is_none());
    assert_eq!(t.feed(b"\x1b[2K").dirty_rows, vec![2]);
    assert!(t.state.grid[2]
        .cells
        .iter()
        .all(|cell| cell.glyph.is_none() && cell.image.is_none() && !cell.spacer));
}
