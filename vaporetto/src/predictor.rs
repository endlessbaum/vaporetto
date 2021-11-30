use std::collections::HashMap;

use crate::char_scorer::CharScorer;
use crate::dict_scorer::DictScorer;
use crate::model::{DictWeight, Model};
use crate::sentence::{BoundaryType, Sentence};
use crate::type_scorer::TypeScorer;

#[cfg(feature = "simd")]
use crate::char_scorer::CharScorerSimd;

/// Predictor.
pub struct Predictor {
    bias: i32,

    char_scorer: CharScorer,
    type_scorer: TypeScorer,
    dict_scorer: Option<DictScorer>,

    quantize_multiplier: f64,

    #[cfg(feature = "simd")]
    padding: usize,
}

impl Predictor {
    /// Creates a new predictor.
    ///
    /// # Arguments
    ///
    /// * `model` - A model data.
    ///
    /// # Returns
    ///
    /// A new predictor.
    pub fn new(model: Model) -> Self {
        let bias = model.bias;

        let char_ngrams = model.char_ngrams;
        let dict = model.dict;
        let dict_weights = model.dict_weights;

        let mut char_ngram_weights = model.char_ngram_weights;
        let type_ngram_weights = model.type_ngram_weights;

        let (dict, dict_weights) = Self::merge_dict_weights(
            dict,
            dict_weights,
            &char_ngrams,
            &mut char_ngram_weights,
            model.char_window_size,
            model.dict_word_wise,
        );

        let char_ngram_weights = Self::merge_weights(&char_ngrams, &char_ngram_weights);
        let type_ngram_weights = Self::merge_weights(&model.type_ngrams, &type_ngram_weights);

        let char_scorer = CharScorer::new(&char_ngrams, char_ngram_weights, model.char_window_size);
        let type_scorer = TypeScorer::new(
            &model.type_ngrams,
            type_ngram_weights,
            model.type_window_size,
        );
        let dict_scorer = if dict.is_empty() {
            None
        } else {
            Some(DictScorer::new(&dict, dict_weights, model.dict_word_wise))
        };

        Self {
            bias,

            char_scorer,
            type_scorer,
            dict_scorer,

            quantize_multiplier: model.quantize_multiplier,

            #[cfg(feature = "simd")]
            padding: model.char_window_size.max(model.type_window_size),
        }
    }

    fn merge_dict_weights(
        dict: Vec<String>,
        dict_weights: Vec<DictWeight>,
        words: &[String],
        word_weights: &mut Vec<Vec<i32>>,
        char_window_size: usize,
        dict_word_wise: bool,
    ) -> (Vec<String>, Vec<DictWeight>) {
        let mut word_map = HashMap::new();
        for (i, word) in words.iter().cloned().enumerate() {
            word_map.insert(word, i);
        }
        let mut new_dict = vec![];
        if dict_word_wise {
            let mut new_dict_weights = vec![];
            for (word, weight) in dict.into_iter().zip(dict_weights) {
                let word_size = word.chars().count();
                match word_map.get(&word) {
                    Some(&idx) if char_window_size >= word_size => {
                        let start = char_window_size - word_size;
                        let end = start + word_size;
                        word_weights[idx][start] += weight.right;
                        for i in start + 1..end {
                            word_weights[idx][i] += weight.inner;
                        }
                        word_weights[idx][end] += weight.left;
                    }
                    _ => {
                        new_dict.push(word);
                        new_dict_weights.push(weight);
                    }
                }
            }
            (new_dict, new_dict_weights)
        } else {
            for word in dict {
                let word_size = word.chars().count();
                match word_map.get(&word) {
                    Some(&idx) if char_window_size >= word_size => {
                        let start = char_window_size - word_size;
                        let end = start + word_size;
                        let word_size_idx = std::cmp::min(word_size, dict_weights.len()) - 1;
                        let weight = &dict_weights[word_size_idx];
                        word_weights[idx][start] += weight.right;
                        for i in start + 1..end {
                            word_weights[idx][i] += weight.inner;
                        }
                        word_weights[idx][end] += weight.left;
                    }
                    _ => new_dict.push(word),
                }
            }
            (new_dict, dict_weights)
        }
    }

    fn merge_weights<P>(words: &[P], weights: &[Vec<i32>]) -> Vec<Vec<i32>>
    where
        P: AsRef<[u8]>,
    {
        let mut result = vec![];
        let word_ids = words
            .iter()
            .enumerate()
            .map(|(i, w)| (w.as_ref().to_vec(), i))
            .collect::<HashMap<Vec<u8>, usize>>();
        for seq in words {
            let seq = seq.as_ref();
            let mut new_weights: Option<Vec<_>> = None;
            for st in (0..seq.len()).rev() {
                if let Some(&idx) = word_ids.get(&seq[st..]) {
                    if let Some(new_weights) = new_weights.as_mut() {
                        for (w_new, w) in new_weights.iter_mut().zip(&weights[idx]) {
                            *w_new += *w;
                        }
                    } else {
                        new_weights.replace(weights[idx].clone());
                    }
                }
            }
            result.push(new_weights.unwrap());
        }
        result
    }

    fn predict_impl(&self, sentence: &Sentence, padding: usize, ys: &mut [i32]) {
        ys.fill(self.bias);
        self.char_scorer.add_scores(sentence, padding, ys);
        self.type_scorer.add_scores(sentence, &mut ys[padding..]);
        if let Some(dict_scorer) = self.dict_scorer.as_ref() {
            dict_scorer.add_scores(sentence, &mut ys[padding..]);
        }
    }

    /// Predicts word boundaries.
    ///
    /// # Arguments
    ///
    /// * `sentence` - A sentence.
    ///
    /// # Returns
    ///
    /// A sentence with predicted boundary information.
    pub fn predict(&self, mut sentence: Sentence) -> Sentence {
        let boundaries_size = sentence.boundaries.len();

        #[cfg(not(feature = "simd"))]
        if boundaries_size != 0 {
            let mut ys = vec![0; boundaries_size];
            self.predict_impl(&sentence, 0, &mut ys);
            for (y, b) in ys.into_iter().zip(sentence.boundaries.iter_mut()) {
                *b = if y >= 0 {
                    BoundaryType::WordBoundary
                } else {
                    BoundaryType::NotWordBoundary
                };
            }
        }

        #[cfg(feature = "simd")]
        if boundaries_size != 0 {
            let ys_size = boundaries_size + self.padding + CharScorerSimd::simd_len() - 1;
            let mut ys = vec![0; ys_size];
            self.predict_impl(&sentence, self.padding, &mut ys);
            for (&y, b) in ys[self.padding..]
                .into_iter()
                .zip(sentence.boundaries.iter_mut())
            {
                *b = if y >= 0 {
                    BoundaryType::WordBoundary
                } else {
                    BoundaryType::NotWordBoundary
                };
            }
        }

        sentence
    }

    /// Predicts word boundaries. This function inserts scores.
    ///
    /// # Arguments
    ///
    /// * `sentence` - A sentence.
    ///
    /// # Returns
    ///
    /// A sentence with predicted boundary information.
    pub fn predict_with_score(&self, mut sentence: Sentence) -> Sentence {
        let boundaries_size = sentence.boundaries.len();

        #[cfg(not(feature = "simd"))]
        if boundaries_size != 0 {
            let mut ys = vec![0; boundaries_size];
            self.predict_impl(&sentence, 0, &mut ys);
            let mut scores = sentence
                .boundary_scores
                .take()
                .unwrap_or_else(|| vec![0.; boundaries_size]);
            for (y, (b, s)) in ys
                .into_iter()
                .zip(sentence.boundaries.iter_mut().zip(scores.iter_mut()))
            {
                *b = if y >= 0 {
                    BoundaryType::WordBoundary
                } else {
                    BoundaryType::NotWordBoundary
                };

                *s = y as f64 * self.quantize_multiplier;
            }
            sentence.boundary_scores.replace(scores);
        }

        #[cfg(feature = "simd")]
        if boundaries_size != 0 {
            let ys_size = boundaries_size + self.padding + CharScorerSimd::simd_len() - 1;
            let mut ys = vec![0; ys_size];
            self.predict_impl(&sentence, self.padding, &mut ys);
            let mut scores = sentence
                .boundary_scores
                .take()
                .unwrap_or_else(|| vec![0.; boundaries_size]);
            for (&y, (b, s)) in ys[self.padding..]
                .into_iter()
                .zip(sentence.boundaries.iter_mut().zip(scores.iter_mut()))
            {
                *b = if y >= 0 {
                    BoundaryType::WordBoundary
                } else {
                    BoundaryType::NotWordBoundary
                };

                *s = y as f64 * self.quantize_multiplier;
            }
            sentence.boundary_scores.replace(scores);
        }

        sentence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Input:  我  ら  は  全  世  界  の  国  民
    /// bias:   -200  ..  ..  ..  ..  ..  ..  ..
    /// words:
    ///   我ら:    3   4   5
    ///   全世界:          6   7   8   9
    ///   国民:                       10  11  12
    ///   世界:           15  16  17  18  19
    ///   界:             20  21  22  23  24  25
    /// types:
    ///   H:      27  28  29
    ///           26  27  28  29
    ///                           26  27  28  29
    ///   K:      32  33
    ///               30  31  32  33
    ///                   30  31  32  33
    ///                       30  31  32  33
    ///                               30  31  32
    ///                                   30  31
    ///   KH:     35  36
    ///                           34  35  36
    ///   HK:         37  38  39
    ///                               37  38  39
    /// dict:
    ///   全世界:         43  44  44  45
    ///   世界:               43  44  45
    ///   世:                 40  42
    fn generate_model_1() -> Model {
        Model {
            char_ngrams: vec![
                "我ら".to_string(),
                "全世界".to_string(),
                "国民".to_string(),
                "世界".to_string(),
                "界".to_string(),
            ],
            type_ngrams: vec![b"H".to_vec(), b"K".to_vec(), b"KH".to_vec(), b"HK".to_vec()],
            dict: vec!["全世界".to_string(), "世界".to_string(), "世".to_string()],
            char_ngram_weights: vec![
                vec![1, 2, 3, 4, 5],
                vec![6, 7, 8, 9],
                vec![10, 11, 12, 13, 14],
                vec![15, 16, 17, 18, 19],
                vec![20, 21, 22, 23, 24, 25],
            ],
            type_ngram_weights: vec![
                vec![26, 27, 28, 29],
                vec![30, 31, 32, 33],
                vec![34, 35, 36],
                vec![37, 38, 39],
            ],
            dict_weights: vec![
                DictWeight {
                    right: 40,
                    inner: 41,
                    left: 42,
                },
                DictWeight {
                    right: 43,
                    inner: 44,
                    left: 45,
                },
            ],
            quantize_multiplier: 0.5,
            dict_word_wise: false,
            bias: -200,
            char_window_size: 3,
            type_window_size: 2,
        }
    }

    /// Input:  我  ら  は  全  世  界  の  国  民
    /// bias:   -285  ..  ..  ..  ..  ..  ..  ..
    /// words:
    ///   我ら:    2   3
    ///   全世界:              4   5
    ///   国民:                            6   7
    ///   世界:                9  10  11
    ///   界:                 12  13  14  15
    /// types:
    ///   H:      18  19  20  21
    ///           17  18  19  20  21
    ///                       16  17  18  19  20
    ///   K:      25  26  27
    ///           22  23  24  25  26  27
    ///               22  23  24  25  26  27
    ///                   22  23  24  25  26  27
    ///                           22  23  24  25
    ///                               22  23  24
    ///   KH:     30  31  32
    ///                       28  29  30  31  32
    ///   HK:     33  34  35  36  37
    ///                           33  34  35  36
    /// dict:
    ///   全世界:         44  45  45  46
    ///   世界:               41  42  43
    ///   世:                 38  40
    fn generate_model_2() -> Model {
        Model {
            char_ngrams: vec![
                "我ら".to_string(),
                "全世界".to_string(),
                "国民".to_string(),
                "世界".to_string(),
                "界".to_string(),
            ],
            type_ngrams: vec![b"H".to_vec(), b"K".to_vec(), b"KH".to_vec(), b"HK".to_vec()],
            dict: vec!["全世界".to_string(), "世界".to_string(), "世".to_string()],
            char_ngram_weights: vec![
                vec![1, 2, 3],
                vec![4, 5],
                vec![6, 7, 8],
                vec![9, 10, 11],
                vec![12, 13, 14, 15],
            ],
            type_ngram_weights: vec![
                vec![16, 17, 18, 19, 20, 21],
                vec![22, 23, 24, 25, 26, 27],
                vec![28, 29, 30, 31, 32],
                vec![33, 34, 35, 36, 37],
            ],
            dict_weights: vec![
                DictWeight {
                    right: 38,
                    inner: 39,
                    left: 40,
                },
                DictWeight {
                    right: 41,
                    inner: 42,
                    left: 43,
                },
                DictWeight {
                    right: 44,
                    inner: 45,
                    left: 46,
                },
            ],
            quantize_multiplier: 0.25,
            dict_word_wise: false,
            bias: -285,
            char_window_size: 2,
            type_window_size: 3,
        }
    }

    /// Input:  我  ら  は  全  世  界  の  国  民
    /// bias:   -285  ..  ..  ..  ..  ..  ..  ..
    /// words:
    ///   我ら:    2   3
    ///   全世界:              4   5
    ///   国民:                            6   7
    ///   世界:                9  10  11
    ///   界:                 12  13  14  15
    /// types:
    ///   H:      18  19  20  21
    ///           17  18  19  20  21
    ///                       16  17  18  19  20
    ///   K:      25  26  27
    ///           22  23  24  25  26  27
    ///               22  23  24  25  26  27
    ///                   22  23  24  25  26  27
    ///                           22  23  24  25
    ///                               22  23  24
    ///   KH:     30  31  32
    ///                       28  29  30  31  32
    ///   HK:     33  34  35  36  37
    ///                           33  34  35  36
    /// dict:
    ///   国民:                           38  39
    ///   世界:               41  42  43
    ///   世:                 44  46
    fn generate_model_3() -> Model {
        Model {
            char_ngrams: vec![
                "我ら".to_string(),
                "全世界".to_string(),
                "国民".to_string(),
                "世界".to_string(),
                "界".to_string(),
            ],
            type_ngrams: vec![b"H".to_vec(), b"K".to_vec(), b"KH".to_vec(), b"HK".to_vec()],
            dict: vec!["国民".to_string(), "世界".to_string(), "世".to_string()],
            char_ngram_weights: vec![
                vec![1, 2, 3],
                vec![4, 5],
                vec![6, 7, 8],
                vec![9, 10, 11],
                vec![12, 13, 14, 15],
            ],
            type_ngram_weights: vec![
                vec![16, 17, 18, 19, 20, 21],
                vec![22, 23, 24, 25, 26, 27],
                vec![28, 29, 30, 31, 32],
                vec![33, 34, 35, 36, 37],
            ],
            dict_weights: vec![
                DictWeight {
                    right: 38,
                    inner: 39,
                    left: 40,
                },
                DictWeight {
                    right: 41,
                    inner: 42,
                    left: 43,
                },
                DictWeight {
                    right: 44,
                    inner: 45,
                    left: 46,
                },
            ],
            quantize_multiplier: 0.25,
            dict_word_wise: true,
            bias: -285,
            char_window_size: 2,
            type_window_size: 3,
        }
    }

    #[test]
    fn test_predict_1() {
        let model = generate_model_1();
        let p = Predictor::new(model);
        let s = Sentence::from_raw("我らは全世界の国民").unwrap();
        let s = p.predict(s);
        assert_eq!(
            &[
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::NotWordBoundary,
            ],
            s.boundaries(),
        );
    }

    #[test]
    fn test_predict_2() {
        let model = generate_model_2();
        let p = Predictor::new(model);
        let s = Sentence::from_raw("我らは全世界の国民").unwrap();
        let s = p.predict(s);
        assert_eq!(
            &[
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
            ],
            s.boundaries(),
        );
    }

    #[test]
    fn test_predict_3() {
        let model = generate_model_3();
        let p = Predictor::new(model);
        let s = Sentence::from_raw("我らは全世界の国民").unwrap();
        let s = p.predict(s);
        assert_eq!(
            &[
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
            ],
            s.boundaries(),
        );
    }

    #[test]
    fn test_predict_with_score_1() {
        let model = generate_model_1();
        let p = Predictor::new(model);
        let s = Sentence::from_raw("我らは全世界の国民").unwrap();
        let s = p.predict_with_score(s);
        assert_eq!(
            &[
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::NotWordBoundary,
            ],
            s.boundaries(),
        );
        assert_eq!(
            &[-38.5, -2.5, 22.5, 66.0, 66.5, 72.0, 25.0, -16.0],
            s.boundary_scores().unwrap(),
        );
    }

    #[test]
    fn test_predict_with_score_2() {
        let model = generate_model_2();
        let p = Predictor::new(model);
        let s = Sentence::from_raw("我らは全世界の国民").unwrap();
        let s = p.predict_with_score(s);
        assert_eq!(
            &[
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
            ],
            s.boundaries(),
        );
        assert_eq!(
            &[-34.5, -27.25, -9.75, 14.25, 26.0, 8.5, -19.75, -28.5],
            s.boundary_scores().unwrap(),
        );
    }

    #[test]
    fn test_predict_with_score_3() {
        let model = generate_model_3();
        let p = Predictor::new(model);
        let s = Sentence::from_raw("我らは全世界の国民").unwrap();
        let s = p.predict_with_score(s);
        assert_eq!(
            &[
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::WordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
                BoundaryType::NotWordBoundary,
            ],
            s.boundaries(),
        );
        assert_eq!(
            &[-34.5, -27.25, -20.75, 4.5, 16.25, -3.0, -10.25, -18.75],
            s.boundary_scores().unwrap(),
        );
    }
}
