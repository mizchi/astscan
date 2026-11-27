use astscan_core::{ParserKind, Registry, RuleSet, Scanner, Severity};

const RULES: &str = "id: log
language: JavaScript
severity: warning
rule:
  pattern: console.log($FIRST, $$$REST)
---
id: diagnostic
language: JavaScript
severity: error
rule:
  pattern: console.log($FIRST, $$$REST)
";

#[test]
fn scans_in_memory_and_reuses_the_rules() {
  let rules = RuleSet::compile(RULES, &mut Registry::default()).unwrap();
  assert_eq!(rules.rule_count(), 2);

  for parser in [ParserKind::TreeSitter, ParserKind::Fast] {
    let mut scanner = Scanner::with_parser(&rules, parser);
    let src = "// ast-grep-ignore: diagnostic\nconsole.log('é', a, b);";
    let slice = |range: std::ops::Range<u32>| &src[range.start as usize..range.end as usize];
    let mut count = 0;
    scanner
      .scan("js", src, |found| {
        count += 1;
        assert_eq!(found.language(), "JavaScript");
        assert_eq!(slice(found.range()), "console.log('é', a, b)");
        assert_eq!(found.rules().map(|rule| (rule.id.as_str(), rule.severity)).collect::<Vec<_>>(), [("log", Severity::Warning)]);
        assert_eq!(found.single_captures().map(|(name, range)| (name, slice(range))).collect::<Vec<_>>(), [("FIRST", "'é'")]);
        assert_eq!(
          found.multi_captures().map(|(name, ranges)| (name, ranges.map(slice).collect::<Vec<_>>())).collect::<Vec<_>>(),
          [("REST", vec!["a", ",", "b"])]
        );
      })
      .unwrap();
    assert_eq!(count, 1);

    let html = "<p>console.log(0)</p><script>console.log(1,2)</script>";
    let mut hits = Vec::new();
    scanner
      .scan("HTML", html, |found| {
        let range = found.range();
        hits.push((&html[range.start as usize..range.end as usize], found.rules().count()));
      })
      .unwrap();
    assert_eq!(hits, [("console.log(1,2)", 2)]);
  }
}

#[test]
fn deeply_nested_source_scans_on_a_small_thread_stack() {
  let rules = RuleSet::compile(RULES, &mut Registry::default()).unwrap();
  std::thread::Builder::new()
    .stack_size(256 * 1024)
    .spawn(move || {
      let call = "console.log(1,2)";
      let js = format!("{}{call}{}", "(".repeat(5000), ")".repeat(5000));
      let mut scanner = Scanner::new(&rules);
      let mut hits = Vec::new();
      scanner.scan("JavaScript", &js, |found| hits.push(found.range())).unwrap();
      assert_eq!(hits.len(), 1);
      assert_eq!(hits[0], 5000..5000 + call.len() as u32);
    })
    .unwrap()
    .join()
    .unwrap();
}
