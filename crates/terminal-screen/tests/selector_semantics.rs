use terminal_screen::*;

// A generated regular tree isolates syntax from segmentation heuristics. Parent
// counts, sibling counts, absent attributes and duplicate values cross boundaries.
fn tree(parents: usize, children: usize) -> ScreenTree {
    let grid = ScreenGrid::new(1, 1, vec![Cell::default()], vec![RowMetadata::default()], Cursor::default(), 0).unwrap();
    let mut tree = analyze(&grid);
    for p in 0..parents {
        let parent = tree.add_node(tree.root(), Node::new("panel", grid.bounds(), "")).unwrap();
        for c in 0..children {
            let mut node = Node::new("item", grid.bounds(), format!("Name {p}-{c}"));
            if c % 3 != 0 {
                node.attributes.insert("state".into(), if c % 3 == 1 { "Ready" } else { "Busy" }.into());
            }
            if c % 2 == 0 {
                node.roles.insert("chosen".into());
            }
            tree.add_node(parent, node).unwrap();
        }
    }
    tree
}

// Owner direction: match positions are global, child positions are per parent,
// `of S` counts only matching siblings; roots have no child position.
#[test]
fn generated_match_and_child_positions() {
    for parents in 1..=4 {
        for children in 1..=7 {
            let t = tree(parents, children);
            assert_eq!(t.select("item:first-child").unwrap().len(), parents);
            assert_eq!(t.select("item:last-child").unwrap().len(), parents);
            assert!(t.select("screen:first-child, screen:last-child").unwrap().is_empty());
            for n in 1..=children + 1 {
                let nodes = t.select(&format!("item:nth-child({n})")).unwrap();
                assert_eq!(nodes.len(), if n <= children { parents } else { 0 });
                assert!(nodes.iter().all(|node| node.text.ends_with(&format!("-{}", n - 1))));
                let chosen = t.select(&format!("item:nth-child({n} of chosen)")).unwrap();
                assert_eq!(chosen.len(), if (n - 1) * 2 < children { parents } else { 0 });
                assert!(chosen.iter().all(|node| node.text.ends_with(&format!("-{}", (n - 1) * 2))));
            }
            assert_eq!(t.select("item:nth-match(1)").unwrap()[0].text, "Name 0-0");
            assert_eq!(t.select("item:last-match").unwrap()[0].text, format!("Name {}-{}", parents - 1, children - 1));
            assert!(t.select(&format!("item:nth-match({})", parents * children + 1)).unwrap().is_empty());
        }
    }
    for bad in [
        "item:nth(1)",
        "item:last",
        "item:not(:nth-match(1))",
        "item:not(:first-child)",
        "item:not(:has(item:last-child))",
        "item:not(:nth-child(2 of chosen))",
        "item:nth-child(0)",
    ] {
        assert!(Selector::parse(bad).is_err(), "{bad}");
    }
}

// Owner direction: sibling relations stay within a parent, and relative :has
// starts at its candidate rather than at that candidate's descendants.
#[test]
fn generated_siblings_and_relative_has() {
    for parents in 1..=3 {
        for children in 1..=6 {
            let t = tree(parents, children);
            assert_eq!(t.select("item + item").unwrap().len(), parents * (children - 1));
            assert_eq!(t.select("item ~ item").unwrap().len(), parents * (children - 1));
            assert_eq!(t.select("item:has(+ item)").unwrap().len(), parents * (children - 1));
            assert_eq!(t.select("item:has(~ item)").unwrap().len(), parents * (children - 1));
            assert!(t.select("item:has(+ panel), item:has(item)").unwrap().is_empty());
            let nodes = t.select("item:has(+ item[state=Ready])").unwrap();
            assert!(nodes.iter().all(|node| node.text.ends_with("-0") || node.text.ends_with("-3")));
            assert_eq!(nodes.len(), parents * (usize::from(children > 1) + usize::from(children > 4)));
            // for 1..6: indices 1 and 4 exist
        }
    }
}

// Unknown comparison attributes fail closed for both forms of inequality;
// boolean absence still permits :not([flag]) for observational style queries.
#[test]
fn absent_values_and_consistent_explicit_case_flags() {
    let t = tree(1, 6);
    let left = Selector::parse("item[state!=Ready]").unwrap().evaluate(&t);
    let right = Selector::parse("item:not([state=Ready])").unwrap().evaluate(&t);
    assert_eq!(left, right);
    assert_eq!(left.len(), 2);
    assert!(t.select("item:not([unknown=x]), item[unknown!=x]").unwrap().is_empty());
    assert_eq!(t.select("item:not([unknown])").unwrap().len(), 6);
    for selector in ["item[state=ready]", "item[state^=ready]", "item[state*=ready]", "item[state$=ready]", "item:has-text('name')"] {
        assert!(t.select(selector).unwrap().is_empty(), "{selector}");
    }
    for selector in ["item[state=ready i]", "item[state^=ready i]", "item[state*=ready i]", "item[state$=ready i]"] {
        assert_eq!(t.select(selector).unwrap().len(), 2, "{selector}");
    }
    assert_eq!(t.select("item:has-text('name' i)").unwrap().len(), 6);
    assert_eq!(t.select("item:matches(/name/i)").unwrap().len(), 6);
    assert_eq!(t.select("item[state!=ready i]").unwrap().len(), 2);
}

// Owner minimum extraction: positive regex predicates expose their named
// captures on each selected node, including Unicode and optional groups.
#[test]
fn named_regex_groups_are_returned_per_match() {
    let t = tree(2, 3);
    let selector = Selector::parse(r"item:matches(/Name (?P<parent>\d+)-(?P<child>\d+)(?P<optional>x)?/)").unwrap();
    let matches = selector.evaluate_with_captures(&t);
    assert_eq!(matches.len(), 6);
    for matched in matches {
        let text = &t.node(matched.node).unwrap().text;
        assert_eq!(text, &format!("Name {}-{}", matched.captures["parent"], matched.captures["child"]));
        assert!(!matched.captures.contains_key("optional"));
    }
    let groups = Selector::parse("item:matches(/(?P<name>Name)/), item:nth-match(1)").unwrap().evaluate_with_captures(&t);
    assert_eq!(groups.len(), 6);
    assert!(groups.iter().all(|m| m.captures["name"] == "Name"));
    assert!(Selector::parse("panel:has(item:matches(/(?P<child>Name)/))")
        .unwrap()
        .evaluate_with_captures(&t)
        .iter()
        .all(|m| m.captures.is_empty()));
}
