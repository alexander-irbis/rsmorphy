use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    fs::File,
    io::Read,
    iter::FromIterator,
    path::{Path, PathBuf},
};

use byteorder::{LittleEndian, ReadBytesExt};
use flate2::read::GzDecoder;
use maplit::hashset;
use serde_json;
use serde_json::Value;
use string_cache::DefaultAtom;

use crate::{
    dawg::{CompletionDawg, Dawg},
    opencorpora::{
        dictionary::*,
        grammeme::{Grammeme, GrammemeReg},
        tag::OpencorporaTagReg,
        dictionary::{Paradigms, ParadigmId, ParadigmIndex}
    },
    util::DumbProfiler,
};

pub use crate::dawg::{HH, HHH};

pub type WordsDawg = CompletionDawg<HH>;
pub type PredictionSuffixesDawg = CompletionDawg<HHH>;
pub type ConditionalProbDistDawg = CompletionDawg<HH>;

/// Open Corpora dictionary wrapper class.
#[derive(Debug)]
pub struct Dictionary {
    pub meta: DictionaryMeta,
    pub grammemes: HashMap<Grammeme, (GrammemeReg, GrammemeMeta)>,
    pub gramtab: Vec<OpencorporaTagReg>,
    pub suffixes: Vec<DefaultAtom>,
    pub paradigms: Paradigms,
    pub words: WordsDawg,
    pub p_t_given_w: ConditionalProbDistDawg,
    pub prediction_prefixes: Dawg,
    pub prediction_suffixes_dawgs: Vec<PredictionSuffixesDawg>,
    pub paradigm_prefixes: Vec<DefaultAtom>,
    pub paradigm_prefixes_rev: Vec<(u16, DefaultAtom)>,
    pub prediction_splits: Vec<usize>,
    pub char_substitutes: BTreeMap<String, String>,
}

impl Dictionary {
    pub fn from_file<P>(p: P) -> Self
    where
        P: AsRef<Path>,
    {
        let load = PathLoader::new(p);

        let mut profiler = DumbProfiler::start();

        let meta = DictionaryMeta::default();

        let meta_json = {
            let meta: Vec<(String, Value)> = load.json("meta.json.gz").expect("object of `Value`s");
            HashMap::<String, Value>::from_iter(meta.into_iter())
        };
        profiler.waypoint("meta");

        let paradigm_prefixes: Vec<DefaultAtom> = {
            meta_json["compile_options"]
                .as_object()
                .unwrap()
                .get("paradigm_prefixes")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().into())
                .collect()
        };
        let max_suffix_length = {
            meta_json.get("prediction_options")
                .unwrap_or_else(|| &meta_json["compile_options"])
                .as_object()
                .unwrap()
                .get("max_suffix_length")
                .unwrap()
                .as_u64()
                .unwrap() as usize
        };
        let prediction_splits = (1..=max_suffix_length).rev().collect();
        profiler.waypoint("meta'");

        let paradigm_prefixes_rev = paradigm_prefixes
            .iter()
            .enumerate()
            .rev()
            .map(|(i, v)| (i as u16, v.clone()))
            .collect();
        profiler.waypoint("paradigm_prefixes_rev");

        let suffixes = load.json("suffixes.json.gz").expect("array of strings");
        profiler.waypoint("suffixes");

        let gramtab: Vec<String> = load
            .json("gramtab-opencorpora-int.json.gz")
            .expect("array of strings");
        profiler.waypoint("gramtab");
        // TODO opencorpora-ext
        let gramtab = gramtab.into_iter().map(OpencorporaTagReg::new).collect();
        profiler.waypoint("gramtab'");

        let grammemes: Vec<Vec<Value>> = load.json("grammemes.json.gz").expect("array of `Value`s");
        profiler.waypoint("grammemes");

        let grammemes = {
            let grammemes = grammemes
                .into_iter()
                .map(GrammemeReg::from_json)
                .enumerate()
                .map(|(index, gr)| {
                    let gr_meta = GrammemeMeta { index, ..GrammemeMeta::default() };
                    (gr.name.clone(), (gr, gr_meta))
                });
            let mut grammemes = HashMap::from_iter(grammemes);

            let gram_regs: Vec<GrammemeReg> = grammemes.values()
                .map(|v| v.0.clone())
                .collect();
            for gram_reg in &gram_regs {
                if let Some(ref parent) = gram_reg.parent {
                    grammemes
                        .get_mut(parent)
                        .expect("Grammeme parent")
                        .1
                        .children
                        .insert(gram_reg.name.clone());
                }
            }

            // Extra incompatible:   'plur': set(['GNdr'])
            // {u'plur': set([u'GNdr', u'masc', u'femn', u'neut'])}
            let plur = Grammeme::new("plur");
            let gndr = Grammeme::new("GNdr");
            let mut extra_incompatible: HashSet<Grammeme> = hashset! { gndr.clone() };
            extra_incompatible.extend(grammemes[&gndr].1.children.iter().cloned());

            for gram_reg in &gram_regs {
                let gm = grammemes.get_mut(&gram_reg.name).unwrap();
                let gm: &mut GrammemeMeta = &mut gm.1;
                if gram_reg.name == plur {
                    gm.incompatible
                        .extend(extra_incompatible.iter()
                            .filter(|&v| *v != gram_reg.name)
                            .cloned()
                        );
                }
                gm.incompatible
                    .extend(gm.children.iter()
                        .filter(|&v| *v != gram_reg.name)
                        .cloned()
                    );
            }

            grammemes
        };
        profiler.waypoint("grammemes'");

        let paradigms = Paradigms::from_reader(&mut load.reader("paradigms.array.gz"));
        profiler.waypoint("paradigms");

        let words = CompletionDawg::from_reader(&mut load.reader("words.dawg.gz"));
        profiler.waypoint("words");

        let p_t_given_w = CompletionDawg::from_reader(&mut load.reader("p_t_given_w.intdawg.gz"));
        profiler.waypoint("p_t_given_w");

        let prediction_prefixes =
            Dawg::from_reader(&mut load.reader("prediction-prefixes.dawg.gz"));
        profiler.waypoint("prediction_prefixes");

        let prediction_suffixes_dawgs = Vec::from_iter((0..paradigm_prefixes.len()).map(|i| {
            CompletionDawg::from_reader(
                &mut load.reader(format!("prediction-suffixes-{}.dawg.gz", i)),
            )
        }));
        profiler.waypoint("prediction_suffixes_dawgs");

        // TODO load char_substitutes
        let char_substitutes = maplit::btreemap! {"е".into() => "ё".into()};

        Dictionary {
            meta,
            grammemes,
            gramtab,
            suffixes,
            paradigms,
            words,
            p_t_given_w,
            prediction_prefixes,
            prediction_suffixes_dawgs,
            paradigm_prefixes,
            paradigm_prefixes_rev,
            prediction_splits,
            char_substitutes,
        }
    }

    #[inline]
    pub fn para(&self) -> &Paradigms{
        &self.paradigms
    }

    /// Return tag as a string
    #[inline]
    pub fn get_tag(&self, id: ParadigmId, idx: ParadigmIndex) -> &OpencorporaTagReg {
        let para = self.para().get(id).expect("paradigm");
        &self.gramtab[para[idx.index()].tag_id as usize]
    }

    /// Return a list of
    ///     (prefix, tag, suffix)
    /// tuples representing the paradigm.
    #[inline]
    pub fn build_paradigm_info(&self, id: ParadigmId) -> Vec<(&str, &OpencorporaTagReg, &str)> {
        Vec::from_iter(self.iter_paradigm_info(id))
    }

    #[inline]
    pub fn iter_paradigm_info<'a: 'i, 'i>(
        &'a self,
        id: ParadigmId,
    ) -> impl Iterator<Item = (&'a str, &'a OpencorporaTagReg, &'a str)> + 'i {
        self.para().get(id.value()).expect("paradigm")
            .iter()
            .map(move |entry: &'a ParadigmEntry| self.paradigm_entry_info(*entry))
    }

    /// Return a tuple
    ///     (prefix, tag, suffix)
    /// tuples representing the paradigm entry.
    #[inline]
    pub fn paradigm_entry_info(&self, entry: ParadigmEntry) -> (&str, &OpencorporaTagReg, &str) {
        (
            &self.paradigm_prefixes[entry.prefix_id as usize],
            &self.gramtab[entry.tag_id as usize],
            &self.suffixes[entry.suffix_id as usize],
        )
    }

    /// Return word stem (given a word, paradigm and the word index).
    #[inline]
    pub fn get_stem<'a>(&self, id: ParadigmId, idx: ParadigmIndex, fixed_word: &'a str) -> &'a str {
        let para = self.para().get(id).expect("paradigm");
        let (prefix, _, suffix) = self.paradigm_entry_info(para[idx.index()]);
        &fixed_word[prefix.len()..fixed_word.len() - suffix.len()]
    }

    /// Build a normal form.
    pub fn build_normal_form<'a>(
        &self,
        id: ParadigmId,
        idx: ParadigmIndex,
        fixed_word: &'a str,
    ) -> Cow<'a, str> {
        if idx.is_first() {
            fixed_word.into()
        } else {
            let stem = self.get_stem(id, idx, fixed_word);
            let para = self.para().get(id).expect("paradigm");
            let (normal_prefix, _, normal_suffix) = self.paradigm_entry_info(para[0]);
            format!("{}{}{}", normal_prefix, stem, normal_suffix).into()
        }
    }

    /// Write a normal form.
    pub fn write_normal_form<'a, W: fmt::Write>(
        &self,
        f: &mut W,
        id: ParadigmId,
        idx: ParadigmIndex,
        fixed_word: &'a str,
    ) -> fmt::Result {
        if idx.is_first() {
            write!(f, "{}", fixed_word)
        } else {
            let stem = self.get_stem(id, idx, fixed_word);
            let para = self.para().get(id).expect("paradigm");
            let (normal_prefix, _, normal_suffix) = self.paradigm_entry_info(para[0]);
            write!(f, "{}{}{}", normal_prefix, stem, normal_suffix)
        }
    }
}