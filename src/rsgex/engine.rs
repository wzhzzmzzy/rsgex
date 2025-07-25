use std::{collections::HashMap, rc::Rc};

use super::{
    matcher::{ClassUnicodeMatcher, EndOfInputMatcher, EpsilonMatcher, StartOfInputMatcher},
    nfa::NFAutomata,
    parser,
};
use anyhow::Result;
use regex_syntax::hir::{Capture, Class, Hir, HirKind, Literal, Look, Repetition};

#[derive(Default)]
pub struct Engine {
    pub nfa: NFAutomata,
}

impl Engine {
    fn alternation(&mut self, ast_vec: &[Hir]) {
        let mut nfa = NFAutomata::new();

        nfa.fill_state(1);
        nfa.set_initial(0);

        ast_vec.iter().for_each(|ast| {
            let sub_nfa = Self::ast_to_nfa(ast.kind());
            nfa.append(&sub_nfa.nfa, 0);
        });

        nfa.fill_state(1);

        let real_ending = nfa.states.len() - 1;
        for from in nfa.ending.clone().into_iter() {
            nfa.add_epsilon_transition(from, real_ending);
            nfa.remove_ending(from);
        }
        nfa.add_ending(real_ending);

        self.nfa = nfa;
    }

    fn concat(&mut self, ast_vec: &[Hir]) {
        let mut nfa = NFAutomata::new();

        nfa.fill_state(1);
        nfa.set_initial(0);
        nfa.add_ending(0);

        ast_vec.iter().for_each(|ast| {
            let sub_nfa = Self::ast_to_nfa(ast.kind());
            let prev_ending = nfa.ending.pop().unwrap();
            nfa.remove_ending(prev_ending);
            nfa.append(&sub_nfa.nfa, prev_ending);
        });

        self.nfa = nfa;
    }

    fn literal(&mut self, literal: &Literal) {
        let mut nfa = NFAutomata::new();

        let len = literal.0.len();
        nfa.declare_state(len + 1, 0, len);
        literal
            .0
            .iter()
            .map(|&b| b as char)
            .enumerate()
            .for_each(|(from, c)| {
                nfa.add_char_transition(from, from + 1, c);
            });

        self.nfa = nfa;
    }

    // + / * / {min, max}
    fn repetition(&mut self, repetition: &Repetition) {
        // nfa: 0
        let mut nfa = NFAutomata::new();
        nfa.fill_state(1);
        nfa.set_initial(0);
        nfa.add_ending(0);

        let sub_nfa = Self::ast_to_nfa(repetition.sub.kind());

        let mut last_sub_nfa_initial: usize;
        // nfa: 0 -> sub_nfa * min -> end
        for _ in 0..repetition.min {
            last_sub_nfa_initial = nfa.ending.pop().unwrap();
            nfa.remove_ending(last_sub_nfa_initial);
            nfa.append(&sub_nfa.nfa, last_sub_nfa_initial);
        }

        // support {min, (max)}
        // nfa: 0 -> sub_nfa * min -> end
        //                         -> sub_nfa -> end
        //                                    -> sub_nfa -> end
        //                                    ...
        if let Some(max) = repetition.max {
            let mut sub_nfa_ending: Vec<usize> = vec![];
            for _ in repetition.min..max {
                let current_sub_nfa_ending = nfa.ending.pop().unwrap();
                nfa.remove_ending(current_sub_nfa_ending);
                nfa.append(&sub_nfa.nfa, current_sub_nfa_ending);
                sub_nfa_ending.push(current_sub_nfa_ending);
            }
            for ending in sub_nfa_ending.into_iter() {
                nfa.add_epsilon_transition(ending, *nfa.ending.last().unwrap());
            }
        }
        // support + / *
        // last sub_nfa may be repeat or skip
        // nfa: 0 -> sub_nfa * min-1 -(last_sub_nfa_initial)> sub_nfa -> end_or_back -> end
        //                                            <(last_sub_nfa_initial)-
        //                                            -(last_sub_nfa_initial)>
        else {
            let mut last_ending = nfa.ending.pop().unwrap();
            nfa.remove_ending(last_ending);
            last_sub_nfa_initial = last_ending;
            nfa.append(&sub_nfa.nfa, last_sub_nfa_initial);

            last_ending = nfa.ending.pop().unwrap();
            nfa.remove_ending(last_ending);

            nfa.fill_state(1);
            let new_ending = nfa.states.len() - 1;
            nfa.add_epsilon_transition(last_ending, last_sub_nfa_initial);
            if repetition.greedy {
                nfa.add_epsilon_transition(last_sub_nfa_initial, last_ending);
                nfa.add_epsilon_transition(last_ending, new_ending);
            } else {
                nfa.unshift_transition(
                    last_sub_nfa_initial,
                    last_ending,
                    Rc::new(EpsilonMatcher {}),
                );
                nfa.unshift_transition(last_ending, new_ending, Rc::new(EpsilonMatcher {}));
            }
            nfa.add_ending(new_ending);
        }

        self.nfa = nfa;
    }

    fn class(&mut self, class: &Class) {
        let mut nfa = NFAutomata::new();
        nfa.fill_state(3);
        nfa.set_initial(0);
        nfa.add_epsilon_transition(0, 1);
        if let Class::Unicode(unicode_range) = class {
            unicode_range.iter().for_each(|r| {
                nfa.add_transition(
                    1,
                    2,
                    Rc::new(ClassUnicodeMatcher {
                        start: r.start(),
                        end: r.end(),
                    }),
                );
            });
        }

        nfa.add_ending(2);

        self.nfa = nfa;
    }

    fn capture(&mut self, capture: &Capture) {
        let mut e = Self::ast_to_nfa(capture.sub.kind());

        e.nfa.mark_capture_group(
            capture.index,
            capture.name.as_ref().map(|n| Rc::from(n.clone())),
        );

        self.nfa = e.nfa;
    }

    fn look(&mut self, look: &Look) {
        let mut nfa = NFAutomata::new();
        nfa.fill_state(2);
        nfa.set_initial(0);
        nfa.add_ending(1);

        match look {
            Look::Start => {
                nfa.add_transition(0, 1, Rc::new(StartOfInputMatcher {}));
            }
            Look::End => {
                nfa.add_transition(0, 1, Rc::new(EndOfInputMatcher {}));
            }
            _ => {
                nfa.add_epsilon_transition(0, 1);
            }
        }

        self.nfa = nfa;
    }

    fn ast_to_nfa(ast: &HirKind) -> Self {
        let mut builder = Self::default();
        match ast {
            HirKind::Alternation(ast_vec) => builder.alternation(ast_vec.as_slice()),
            HirKind::Concat(ast_vec) => builder.concat(ast_vec.as_slice()),
            HirKind::Literal(literal) => builder.literal(literal),
            HirKind::Repetition(repetition) => builder.repetition(repetition),
            HirKind::Class(class) => builder.class(class),
            HirKind::Capture(capture) => builder.capture(capture),
            HirKind::Look(look) => builder.look(look),
            _ => (),
        }

        println!("ast_to_nfa, {:?}", ast);

        builder
    }

    pub fn exec(&self, s: &str) -> Option<HashMap<String, String>> {
        self.nfa.compute(s)
    }

    pub(crate) fn exec_test(&self, s: &str) -> String {
        self.exec(s).unwrap().get(&0.to_string()).unwrap().clone()
    }

    pub fn test(&self, s: &str) -> bool {
        self.nfa.compute(s).is_some()
    }
}

impl TryFrom<&str> for Engine {
    type Error = anyhow::Error;

    fn try_from(pattern: &str) -> Result<Engine, Self::Error> {
        let ast = parser::parse_by_regex_syntax(pattern);
        let mut e = Engine::ast_to_nfa(ast?.kind());

        e.nfa.mark_capture_group(0, None);
        e.nfa.debug();

        Ok(e)
    }
}

#[cfg(test)]
mod test {
    use super::Engine;

    #[test]
    fn test_literal() {
        let e = Engine::try_from("123").unwrap();
        assert!(e.nfa.compute("123").is_some());
        assert!(e.nfa.compute("124").is_none());
    }

    #[test]
    fn test_alternation() {
        let e = Engine::try_from("123|456").unwrap();
        assert!(e.nfa.compute("123").is_some());
        assert!(e.nfa.compute("456").is_some());
        assert!(e.nfa.compute("345").is_none());
    }

    #[test]
    fn test_repetition() {
        let e = Engine::try_from("1+").unwrap();

        assert!(e.nfa.compute("1").is_some());
        assert!(e.nfa.compute("11").is_some());
        assert!(e.nfa.compute("111").is_some());
    }

    #[test]
    fn test_repetition_0_any() {
        let e = Engine::try_from("01*").unwrap();

        assert_eq!(e.exec_test("0"), "0");
        assert_eq!(e.exec_test("01"), "01");
        assert_eq!(e.exec_test("011"), "011");
    }

    #[test]
    fn test_repetition_lazy() {
        let e = Engine::try_from("01+?").unwrap();

        assert_eq!(e.exec_test("01111"), "01");
    }

    #[test]
    fn test_repetition_complex() {
        let e = Engine::try_from("1+2+3+4{2}").unwrap();

        assert!(e.nfa.compute("11122233344").is_some());
        assert!(e.nfa.compute("111222333445").is_some());
        assert!(e.nfa.compute("22244").is_none());
        assert!(e.nfa.compute("11122233345").is_none());

        let e2 = Engine::try_from("1234{1,5}").unwrap();

        assert!(e2.nfa.compute("123444455").is_some());
    }

    #[test]
    fn test_accepter() {
        let e = Engine::try_from("123+|456+|7{3}|888").unwrap();

        assert!(e.nfa.compute("888").is_some());
    }

    #[test]
    fn test_capture_group() {
        let e = Engine::try_from("(?<all>e(a)e)").unwrap();

        let res = e.nfa.compute("eae");
        assert!(res.is_some());

        assert_eq!(
            Some("eae"),
            res.as_ref()
                .unwrap()
                .get(&0.to_string())
                .map(|s| s.as_str())
        );
        assert_eq!(
            Some("eae"),
            res.as_ref()
                .unwrap()
                .get(&"all".to_string())
                .map(|s| s.as_str())
        );
        assert_eq!(
            Some("a"),
            res.as_ref()
                .unwrap()
                .get(&2.to_string())
                .map(|s| s.as_str())
        );
    }

    #[test]
    fn test_class() {
        let e = Engine::try_from("[1-9]+").unwrap();

        assert_eq!(e.exec_test("1"), "1");
        assert_eq!(e.exec_test("12"), "12");

        let e = Engine::try_from("[^1-9]").unwrap();

        println!("{:?}", e.nfa.compute("0"));
        assert_eq!(e.exec_test("0"), "0");

        let e = Engine::try_from("[^1-9]+").unwrap();
        assert_eq!(e.exec_test("0"), "0");
        assert!(e.nfa.compute("1").is_none());
    }

    #[test]
    fn test_look() {
        let e = Engine::try_from("123$").unwrap();

        assert_eq!(e.exec_test("123"), "123");
        assert!(e.nfa.compute("1234").is_none());
    }
}
