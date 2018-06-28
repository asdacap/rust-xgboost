use libc;
use std::{fs::File, io::Write, fmt, mem, slice, ffi, ptr};
use std::collections::BTreeMap;
use std::path::Path;
use error::XGBError;
use dmatrix::DMatrix;
use std::os::unix::ffi::OsStrExt;

use ndarray;
use xgboost_sys;
use tempfile;

use super::XGBResult;
use parameters::Parameters;
use parameters::learning::{Objective, EvaluationMetric, Metrics};

enum PredictOption {
    OutputMargin,
    PredictLeaf,
    PredictContribitions,
    //ApproximateContributions,
    PredictInteractions,
}

impl PredictOption {
    fn options_as_mask(options: &[PredictOption]) -> i32 {
        let mut option_mask = 0x00;
        for option in options {
            let value = match *option {
                PredictOption::OutputMargin => 0x01,
                PredictOption::PredictLeaf => 0x02,
                PredictOption::PredictContribitions => 0x04,
                //PredictOption::ApproximateContributions => 0x08,
                PredictOption::PredictInteractions => 0x10,
            };
            option_mask |= value;
        }

        option_mask
    }
}

/// This is the core model in XGBoost, containing functions for training, evaluating and predicting.
pub struct Booster {
    handle: xgboost_sys::BoosterHandle,
}

impl Booster {
    /// Convenience function for creating/training a new Booster.
    pub fn train(
        params: &Parameters,
        dtrain: &DMatrix,
        num_boost_round: u32,
        eval_sets: &[(&DMatrix, &str)],
    ) -> XGBResult<Self> {
        let dmats = {
            let mut dmats = vec![dtrain];
            for (dmat, _) in eval_sets {
                dmats.push(dmat);
            }
            dmats
        };

        let mut bst = Booster::create(&dmats, &params)?;
        let num_parallel_tree = 1;

        // load distributed code checkpoint from rabit
        let version = bst.load_rabit_checkpoint()?;
        debug!("Loaded Rabit checkpoint: version={}", version);
        assert!(unsafe { xgboost_sys::RabitGetWorldSize() != 1 || version == 0 });

        let rank = unsafe { xgboost_sys::RabitGetRank() };
        let start_iteration = version / 2;
        let mut nboost = start_iteration;

        let custom_eval_funcs = {
            let mut eval_funcs = Vec::new();
            if let Metrics::Custom(ref eval_metrics) = params.learning_params.eval_metrics {
                for eval_metric in eval_metrics {
                    if let EvaluationMetric::Custom(name, eval_func) = eval_metric {
                        eval_funcs.push((name, eval_func));
                    }
                }
            }
            eval_funcs
        };

        for i in start_iteration..num_boost_round as i32 {
            // distributed code: need to resume to this point
            // skip first update if a recovery step
            if version % 2 == 0 {
                if let Objective::Custom(objective_fn) = params.learning_params.objective {
                    let pred = bst.predict(dtrain)?;
                    let (gradient, hessian) = objective_fn(&pred.to_vec(), dtrain);
                    bst.boost(dtrain, &gradient, &hessian)?;
                } else {
                    bst.update(&dtrain, i)?;
                }
                bst.save_rabit_checkpoint()?;
            }

            assert!(unsafe { xgboost_sys::RabitGetWorldSize() == 1 || version == xgboost_sys::RabitVersionNumber() });

            nboost += 1;

            if !eval_sets.is_empty() {
                let mut dmat_eval_results = bst.eval_set(eval_sets, i)?;

                if !custom_eval_funcs.is_empty() {
                    for (dmat, dmat_name) in eval_sets {
                        let margin = bst.predict_margin(dmat)?;
                        for (eval_name, eval_func) in &custom_eval_funcs {
                            let eval_result = eval_func(&margin.as_slice().unwrap(), dmat);
                            let mut eval_results = dmat_eval_results.entry(dmat_name.to_string()).or_insert_with(BTreeMap::new);
                            eval_results.insert(eval_name.to_string(), eval_result);
                        }
                    }
                }

                // convert to map of eval_name -> (dmat_name -> score)
                let mut eval_dmat_results = BTreeMap::new();
                for (dmat_name, eval_results) in &dmat_eval_results {
                    for (eval_name, result) in eval_results {
                        let mut dmat_results = eval_dmat_results.entry(eval_name).or_insert_with(BTreeMap::new);
                        dmat_results.insert(dmat_name, result);
                    }
                }

                print!("[{}]", i);
                for (eval_name, dmat_results) in eval_dmat_results {
                    for (dmat_name, result) in dmat_results {
                        print!("\t{}-{}:{}", dmat_name, eval_name, result);
                    }
                }
                println!();
            }
        }

        Ok(bst)
    }

    /// Save this Booster model as a binary file at given path.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> XGBResult<()> {
        debug!("Writing Booster to: {}", path.as_ref().display());
        let fname = ffi::CString::new(path.as_ref().as_os_str().as_bytes()).unwrap();
        xgb_call!(xgboost_sys::XGBoosterSaveModel(self.handle, fname.as_ptr()))
    }

    /// Load a Booster model from a binary file at given path.
    pub fn load<P: AsRef<Path>>(path: P) -> XGBResult<Self> {
        debug!("Loading Booster from: {}", path.as_ref().display());

        // gives more control over error messages, avoids stack trace dump from C++
        if !path.as_ref().exists() {
            return Err(XGBError::new(format!("File not found: {}", path.as_ref().display())));
        }

        let fname = ffi::CString::new(path.as_ref().as_os_str().as_bytes()).unwrap();
        let mut handle = ptr::null_mut();
        xgb_call!(xgboost_sys::XGBoosterCreate(ptr::null(), 0, &mut handle))?;
        xgb_call!(xgboost_sys::XGBoosterLoadModel(handle, fname.as_ptr()))?;
        Ok(Booster { handle })
    }

    // TODO: find out what cached matrices do here
    pub fn create(dmats: &[&DMatrix], params: &Parameters) -> XGBResult<Self> {
        let mut handle = ptr::null_mut();
        // TODO: check this is safe, if any dmats are freed
        let s: Vec<xgboost_sys::DMatrixHandle> = dmats.iter().map(|x| x.handle).collect();
        xgb_call!(xgboost_sys::XGBoosterCreate(s.as_ptr(), dmats.len() as u64, &mut handle))?;

        let mut booster = Booster { handle };
        booster.set_params(params)?;
        Ok(booster)
    }

    pub fn set_params(&mut self, p: &Parameters) -> XGBResult<()> {
        for (key, value) in p.as_string_pairs() {
            debug!("Setting parameter: {}={}", &key, &value);
            self.set_param(&key, &value)?;
        }
        Ok(())
    }

    pub fn update(&mut self, dtrain: &DMatrix, iteration: i32) -> XGBResult<()> {
        xgb_call!(xgboost_sys::XGBoosterUpdateOneIter(self.handle, iteration, dtrain.handle))
    }

    pub fn boost(&mut self, dtrain: &DMatrix, grad: &[f32], hess: &[f32]) -> XGBResult<()> {
        if grad.len() != hess.len() {
            let msg = format!("Mismatch between length of gradient and hessian arrays ({} != {})",
                              grad.len(), hess.len());
            return Err(XGBError::new(msg));
        }
        assert_eq!(grad.len(), hess.len());

        // TODO: _validate_feature_names
        let mut grad_vec = grad.to_vec();
        let mut hess_vec = hess.to_vec();
        xgb_call!(xgboost_sys::XGBoosterBoostOneIter(self.handle,
                                                     dtrain.handle,
                                                     grad_vec.as_mut_ptr(),
                                                     hess_vec.as_mut_ptr(),
                                                     grad_vec.len() as u64))
    }

    // TODO: cleaner to just accept a single DMatrix, no names, no iteration, and parse/return results
    // in some structured format? E.g. BTreeMap<String, f32>?
    fn eval_set(&self, evals: &[(&DMatrix, &str)], iteration: i32) -> XGBResult<BTreeMap<String, BTreeMap<String, f32>>> {
        let (dmats, names) = {
            let mut dmats = Vec::with_capacity(evals.len());
            let mut names = Vec::with_capacity(evals.len());
            for (dmat, name) in evals {
                dmats.push(dmat);
                names.push(*name);
            }
            (dmats, names)
        };
        assert_eq!(dmats.len(), names.len());

        let mut s: Vec<xgboost_sys::DMatrixHandle> = dmats.iter().map(|x| x.handle).collect();

        let mut evnames: Vec<*const libc::c_char> = {
            let mut evnames = Vec::new();
            for name in &names {
                let cstr = ffi::CString::new(*name).unwrap();
                evnames.push(cstr.as_ptr());
                mem::forget(cstr);
            }
            evnames
        };
        evnames.shrink_to_fit();
        let mut out_result = ptr::null();
        xgb_call!(xgboost_sys::XGBoosterEvalOneIter(self.handle,
                                                    iteration,
                                                    s.as_mut_ptr(),
                                                    evnames.as_mut_ptr(),
                                                    dmats.len() as u64,
                                                    &mut out_result))?;
        let out = unsafe { ffi::CStr::from_ptr(out_result).to_str().unwrap().to_owned() };
        Ok(Booster::parse_eval_string(&out, &names))
    }

    /// Evaluate given matrix against
    pub fn evaluate(&self, dmat: &DMatrix) -> XGBResult<BTreeMap<String, f32>> {
        let name = "default";
        let mut eval = self.eval_set(&[(dmat, name)], 0)?;
        Ok(eval.remove(name).unwrap())
    }

    fn get_attribute_names(&self) -> XGBResult<Vec<String>> {
        let mut out_len = 0;
        let mut out = ptr::null_mut();
        xgb_call!(xgboost_sys::XGBoosterGetAttrNames(self.handle, &mut out_len, &mut out))?;

        let out_ptr_slice = unsafe { slice::from_raw_parts(out, out_len as usize) };
        let out_vec = out_ptr_slice.iter()
            .map(|str_ptr| unsafe { ffi::CStr::from_ptr(*str_ptr).to_str().unwrap().to_owned() })
            .collect();
        Ok(out_vec)
    }

    /// Predict results for given data.
    ///
    /// Returns an array containing one entry per row in the given data.
    pub fn predict(&self, dmat: &DMatrix) -> XGBResult<ndarray::Array1<f32>> {
        let option_mask = PredictOption::options_as_mask(&[]);
        let ntree_limit = 0;
        let mut out_len = 0;
        let mut out_result = ptr::null();
        xgb_call!(xgboost_sys::XGBoosterPredict(self.handle,
                                                dmat.handle,
                                                option_mask,
                                                ntree_limit,
                                                &mut out_len,
                                                &mut out_result))?;

        let s = unsafe { slice::from_raw_parts(out_result, out_len as usize).to_vec() };
        Ok(ndarray::Array1::from_vec(s))
    }

    /// Predict margin for given data.
    ///
    /// Returns an array containing one entry per row in the given data.
    pub fn predict_margin(&self, dmat: &DMatrix) -> XGBResult<ndarray::Array1<f32>> {
        let option_mask = PredictOption::options_as_mask(&[PredictOption::OutputMargin]);
        let ntree_limit = 0;
        let mut out_len = 0;
        let mut out_result = ptr::null();
        xgb_call!(xgboost_sys::XGBoosterPredict(self.handle,
                                                dmat.handle,
                                                option_mask,
                                                ntree_limit,
                                                &mut out_len,
                                                &mut out_result))?;

        let s = unsafe { slice::from_raw_parts(out_result, out_len as usize).to_vec() };
        Ok(ndarray::Array1::from_vec(s))
    }

    /// Get predicted leaf index for each sample in given data.
    ///
    /// Returns an array of shape (number of samples, number of trees).
    ///
    /// Note: the leaf index of a tree is unique per tree, so e.g. leaf 1 could be found in both tree 1 and tree 0.
    pub fn predict_leaf(&self, dmat: &DMatrix) -> XGBResult<ndarray::Array2<f32>> {
        let option_mask = PredictOption::options_as_mask(&[PredictOption::PredictLeaf]);
        let ntree_limit = 0;
        let mut out_len = 0;
        let mut out_result = ptr::null();
        xgb_call!(xgboost_sys::XGBoosterPredict(self.handle,
                                                dmat.handle,
                                                option_mask,
                                                ntree_limit,
                                                &mut out_len,
                                                &mut out_result))?;

        let s = unsafe { slice::from_raw_parts(out_result, out_len as usize).to_vec() };
        let num_rows = dmat.num_rows();

        // TODO: re-wrap error as XGBResult
        Ok(ndarray::Array2::from_shape_vec((num_rows, s.len() / num_rows), s).unwrap())
    }

    /// Get feature contributions (SHAP values) for each prediction.
    ///
    /// The sum of all feature contributions is equal to the run untransformed margin value of the
    /// prediction.
    ///
    /// Returns an array of shape (number of samples, number of features + 1). The final column contains the
    /// bias term.
    pub fn predict_contributions(&self, dmat: &DMatrix) -> XGBResult<ndarray::Array2<f32>> {
        let option_mask = PredictOption::options_as_mask(&[PredictOption::PredictContribitions]);
        let ntree_limit = 0;
        let mut out_len = 0;
        let mut out_result = ptr::null();
        xgb_call!(xgboost_sys::XGBoosterPredict(self.handle,
                                                dmat.handle,
                                                option_mask,
                                                ntree_limit,
                                                &mut out_len,
                                                &mut out_result))?;

        let s = unsafe { slice::from_raw_parts(out_result, out_len as usize).to_vec() };
        let num_rows = dmat.num_rows();

        // TODO: re-wrap error as XGBResult
        Ok(ndarray::Array2::from_shape_vec((num_rows, s.len() / num_rows), s).unwrap())
    }

    /// Get SHAP interaction values for each pair of features for each prediction.
    ///
    /// The sum of each row (or column) of the interaction values equals the corresponding SHAP
    /// value (from `predict_contributions`), and the sum of the entire matrix equals the raw
    /// untransformed margin value of the prediction.
    ///
    /// Returns an array of shape (number of samples, number of features + 1, number of features + 1).
    /// The final row and column contain the bias terms.
    pub fn predict_interactions(&self, dmat: &DMatrix) -> XGBResult<ndarray::Array3<f32>> {
        let option_mask = PredictOption::options_as_mask(&[PredictOption::PredictInteractions]);
        let ntree_limit = 0;
        let mut out_len = 0;
        let mut out_result = ptr::null();
        xgb_call!(xgboost_sys::XGBoosterPredict(self.handle,
                                                dmat.handle,
                                                option_mask,
                                                ntree_limit,
                                                &mut out_len,
                                                &mut out_result))?;

        let s = unsafe { slice::from_raw_parts(out_result, out_len as usize).to_vec() };
        let num_rows = dmat.num_rows();

        // TODO: re-wrap error as XGBResult
        let dim = ((s.len() / num_rows) as f64).sqrt() as usize;
        Ok(ndarray::Array3::from_shape_vec((num_rows, dim, dim), s).unwrap())
    }

    pub fn get_attribute(&self, key: &str) -> XGBResult<Option<String>> {
        let key = ffi::CString::new(key).unwrap();
        let mut out_buf = ptr::null();
        let mut success = 0;
        xgb_call!(xgboost_sys::XGBoosterGetAttr(self.handle, key.as_ptr(), &mut out_buf, &mut success))?;
        if success == 0 {
            return Ok(None);
        }
        assert!(success == 1);

        let c_str: &ffi::CStr = unsafe { ffi::CStr::from_ptr(out_buf) };
        let out = c_str.to_str().unwrap();
        Ok(Some(out.to_owned()))
    }

    pub fn dump_model<I>(&self, feature_map: I) -> XGBResult<()>
    where
        I: IntoIterator<Item=(usize, Feature)>
    {
        let tmp_dir = tempfile::tempdir().unwrap(); // FIXME: error handling
        let file_path = tmp_dir.path().join("fmap.txt");
        let mut file = File::create(&file_path).unwrap(); // FIXME: error handling
        for (feature_num, feature) in feature_map.into_iter() {
            let (name, type_indicator) = match feature {
                Feature::Binary(s) => (s, "i"),
                Feature::Quantitative(s) => (s, "q"),
                Feature::Integer(s) => (s, "int"),
            };
            write!(file, "{}\t{}\t{}", feature_num, name, type_indicator).unwrap();
        }

        let fmap = ffi::CString::new(file_path.as_os_str().as_bytes()).unwrap();
        let with_stats = true;
        let format = ffi::CString::new("text").unwrap();
        let mut out_len = 0;
        let mut out_dump_array = ptr::null_mut();
        xgb_call!(xgboost_sys::XGBoosterDumpModelEx(self.handle,
                                                    fmap.as_ptr(),
                                                    with_stats as i32,
                                                    format.as_ptr(),
                                                    &mut out_len,
                                                    &mut out_dump_array))?;

        let out_ptr_slice = unsafe { slice::from_raw_parts(out_dump_array, out_len as usize) };
        let out_vec: Vec<String> = out_ptr_slice.iter()
            .map(|str_ptr| unsafe { ffi::CStr::from_ptr(*str_ptr).to_str().unwrap().to_owned() })
            .collect();

        assert_eq!(out_len as usize, out_vec.len());
        for s in &out_vec {
            println!("{}", s);
        }
        Ok(())
    }

    pub(crate) fn load_rabit_checkpoint(&self) -> XGBResult<i32> {
        let mut version = 0;
        xgb_call!(xgboost_sys::XGBoosterLoadRabitCheckpoint(self.handle, &mut version))?;
        Ok(version)
    }

    pub(crate) fn save_rabit_checkpoint(&self) -> XGBResult<()> {
        xgb_call!(xgboost_sys::XGBoosterSaveRabitCheckpoint(self.handle))
    }

    fn set_attribute(&mut self, key: &str, value: &str) -> XGBResult<()> {
        let key = ffi::CString::new(key).unwrap();
        let value = ffi::CString::new(value).unwrap();
        xgb_call!(xgboost_sys::XGBoosterSetAttr(self.handle, key.as_ptr(), value.as_ptr()))
    }

    fn set_param(&mut self, name: &str, value: &str) -> XGBResult<()> {
        let name = ffi::CString::new(name).unwrap();
        let value = ffi::CString::new(value).unwrap();
        xgb_call!(xgboost_sys::XGBoosterSetParam(self.handle, name.as_ptr(), value.as_ptr()))
    }

    fn parse_eval_string(eval: &str, evnames: &[&str]) -> BTreeMap<String, BTreeMap<String, f32>> {
        let mut result: BTreeMap<String, BTreeMap<String, f32>> = BTreeMap::new();

        for part in eval.split('\t').skip(1) {
            for evname in evnames {
                if part.starts_with(evname) {
                    let metric_parts: Vec<&str> = part[evname.len()+1..].split(':').into_iter().collect();
                    assert_eq!(metric_parts.len(), 2);
                    let metric = metric_parts[0];
                    let score = metric_parts[1].parse::<f32>()
                        .expect(&format!("Unable to parse XGBoost metrics output: {}", eval));

                    let mut metric_map = result.entry(evname.to_string()).or_insert_with(BTreeMap::new);
                    metric_map.insert(metric.to_owned(), score);
                }
            }
        }

        result
    }

}

impl Drop for Booster {
    fn drop(&mut self) {
        xgb_call!(xgboost_sys::XGBoosterFree(self.handle)).unwrap();
    }
}

/// Indicates the type of a feature.
pub enum Feature {
    /// Binary indicator feature.
    Binary(String),

    /// Quantitative feature (e.g. age, time, etc.), can be missing.
    Quantitative(String),

    /// Integer feature (when hinted, decision boundary will be integer).
    Integer(String),
}

impl Feature {
    fn from_type<S: Into<String>>(name: S, feature_type: &str) -> Result<Self, String> {
        match feature_type {
            "i"   => Ok(Feature::Binary(name.into())),
            "q"   => Ok(Feature::Quantitative(name.into())),
            "int" => Ok(Feature::Integer(name.into())),
            _     => Err(format!("Unrecognised feature type '{}', must be one of: 'i', 'q', 'int'", feature_type))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, BufRead};
    use super::*;
    use parameters::{self, learning, tree};

    fn read_train_matrix() -> XGBResult<DMatrix> {
        DMatrix::load("xgboost-sys/xgboost/demo/data/agaricus.txt.train")
    }

    fn load_test_booster() -> Booster {
        let dmat = read_train_matrix().expect("Reading train matrix failed");
        Booster::create(&[&dmat], &Parameters::default()).expect("Creating Booster failed")
    }

    #[test]
    fn set_booster_param() {
        let mut booster = load_test_booster();
        let res = booster.set_param("key", "value");
        assert!(res.is_ok());
    }

    #[test]
    fn load_rabit_version() {
        let version = load_test_booster().load_rabit_checkpoint().unwrap();
        assert_eq!(version, 0);
    }

    #[test]
    fn get_set_attr() {
        let mut booster = load_test_booster();
        let attr = booster.get_attribute("foo").expect("Getting attribute failed");
        assert_eq!(attr, None);

        booster.set_attribute("foo", "bar").expect("Setting attribute failed");
        let attr = booster.get_attribute("foo").expect("Getting attribute failed");
        assert_eq!(attr, Some("bar".to_owned()));
    }

    #[test]
    fn get_attribute_names() {
        let mut booster = load_test_booster();
        let attrs = booster.get_attribute_names().expect("Getting attributes failed");
        assert_eq!(attrs, Vec::<String>::new());

        booster.set_attribute("foo", "bar").expect("Setting attribute failed");
        booster.set_attribute("another", "another").expect("Setting attribute failed");
        booster.set_attribute("4", "4").expect("Setting attribute failed");
        booster.set_attribute("an even longer attribute name?", "").expect("Setting attribute failed");

        let mut expected = vec!["foo", "another", "4", "an even longer attribute name?"];
        expected.sort();
        let mut attrs = booster.get_attribute_names().expect("Getting attributes failed");
        attrs.sort();
        assert_eq!(attrs, expected);
    }

    #[test]
    fn predict() {
        let dmat_train = DMatrix::load("xgboost-sys/xgboost/demo/data/agaricus.txt.train").unwrap();
        let dmat_test = DMatrix::load("xgboost-sys/xgboost/demo/data/agaricus.txt.test").unwrap();

        let tree_params = tree::TreeBoosterParametersBuilder::default()
            .max_depth(2)
            .eta(1.0)
            .build()
            .unwrap();
        let learning_params = learning::LearningTaskParametersBuilder::default()
            .objective(learning::Objective::BinaryLogistic)
            .eval_metrics(learning::Metrics::Custom(vec![learning::EvaluationMetric::MAPCutNegative(4),
                                                         learning::EvaluationMetric::LogLoss,
                                                         learning::EvaluationMetric::BinaryErrorRate(0.5)]))
            .build()
            .unwrap();
        let params = parameters::ParametersBuilder::default()
            .booster_params(parameters::booster::BoosterParameters::GbTree(tree_params))
            .learning_params(learning_params)
            .silent(true)
            .build()
            .unwrap();
        let mut booster = Booster::create(&[&dmat_train, &dmat_test], &params).unwrap();

        for i in 0..10 {
            booster.update(&dmat_train, i).expect("update failed");
        }

        let train_metrics = booster.evaluate(&dmat_train).unwrap();
        assert_eq!(*train_metrics.get("logloss").unwrap(), 0.006634);
        assert_eq!(*train_metrics.get("map@4-").unwrap(), 0.001274);

        let test_metrics = booster.evaluate(&dmat_test).unwrap();
        assert_eq!(*test_metrics.get("logloss").unwrap(), 0.00692);
        assert_eq!(*test_metrics.get("map@4-").unwrap(), 0.005155);

        let v = booster.predict(&dmat_test).unwrap();
        assert_eq!(v.len(), dmat_test.num_rows());

        // first 10 predictions
        let expected_start = [0.0050151693,
                              0.9884467,
                              0.0050151693,
                              0.0050151693,
                              0.026636455,
                              0.11789363,
                              0.9884467,
                              0.01231471,
                              0.9884467,
                              0.00013656063];

        // last 10 predictions
        let expected_end = [0.002520344,
                            0.00060917926,
                            0.99881005,
                            0.00060917926,
                            0.00060917926,
                            0.00060917926,
                            0.00060917926,
                            0.9981102,
                            0.002855195,
                            0.9981102];
        let eps = 1e-6;

        for (pred, expected) in v.iter().zip(&expected_start) {
            println!("predictions={}, expected={}", pred, expected);
            assert!(pred - expected < eps);
        }

        for (pred, expected) in v.slice(s![-10..]).iter().zip(&expected_end) {
            println!("predictions={}, expected={}", pred, expected);
            assert!(pred - expected < eps);
        }
    }

    #[test]
    fn predict_leaf() {
        let dmat_train = DMatrix::load("xgboost-sys/xgboost/demo/data/agaricus.txt.train").unwrap();
        let dmat_test = DMatrix::load("xgboost-sys/xgboost/demo/data/agaricus.txt.test").unwrap();

        let tree_params = tree::TreeBoosterParametersBuilder::default()
            .max_depth(2)
            .eta(1.0)
            .build()
            .unwrap();
        let learning_params = learning::LearningTaskParametersBuilder::default()
            .objective(learning::Objective::BinaryLogistic)
            .eval_metrics(learning::Metrics::Custom(vec![learning::EvaluationMetric::LogLoss]))
            .build()
            .unwrap();
        let params = parameters::ParametersBuilder::default()
            .booster_params(parameters::booster::BoosterParameters::GbTree(tree_params))
            .learning_params(learning_params)
            .silent(true)
            .build()
            .unwrap();
        let mut booster = Booster::create(&[&dmat_train, &dmat_test], &params).unwrap();

        let num_rounds = 15;
        for i in 0..num_rounds {
            booster.update(&dmat_train, i).expect("update failed");
        }

        let preds = booster.predict_leaf(&dmat_test).unwrap();
        let num_samples = dmat_test.num_rows();
        assert_eq!(preds.shape(), &[num_samples, num_rounds as usize]);
    }

    #[test]
    fn predict_contributions() {
        let dmat_train = DMatrix::load("xgboost-sys/xgboost/demo/data/agaricus.txt.train").unwrap();
        let dmat_test = DMatrix::load("xgboost-sys/xgboost/demo/data/agaricus.txt.test").unwrap();

        let tree_params = tree::TreeBoosterParametersBuilder::default()
            .max_depth(2)
            .eta(1.0)
            .build()
            .unwrap();
        let learning_params = learning::LearningTaskParametersBuilder::default()
            .objective(learning::Objective::BinaryLogistic)
            .eval_metrics(learning::Metrics::Custom(vec![learning::EvaluationMetric::LogLoss]))
            .build()
            .unwrap();
        let params = parameters::ParametersBuilder::default()
            .booster_params(parameters::booster::BoosterParameters::GbTree(tree_params))
            .learning_params(learning_params)
            .silent(true)
            .build()
            .unwrap();
        let mut booster = Booster::create(&[&dmat_train, &dmat_test], &params).unwrap();

        let num_rounds = 5;
        for i in 0..num_rounds {
            booster.update(&dmat_train, i).expect("update failed");
        }

        let preds = booster.predict_contributions(&dmat_test).unwrap();
        let num_samples = dmat_test.num_rows();
        let num_features = dmat_train.num_cols();
        assert_eq!(preds.shape(), &[num_samples, num_features + 1]);
    }

    #[test]
    fn predict_interactions() {
        let dmat_train = DMatrix::load("xgboost-sys/xgboost/demo/data/agaricus.txt.train").unwrap();
        let dmat_test = DMatrix::load("xgboost-sys/xgboost/demo/data/agaricus.txt.test").unwrap();

        let tree_params = tree::TreeBoosterParametersBuilder::default()
            .max_depth(2)
            .eta(1.0)
            .build()
            .unwrap();
        let learning_params = learning::LearningTaskParametersBuilder::default()
            .objective(learning::Objective::BinaryLogistic)
            .eval_metrics(learning::Metrics::Custom(vec![learning::EvaluationMetric::LogLoss]))
            .build()
            .unwrap();
        let params = parameters::ParametersBuilder::default()
            .booster_params(parameters::booster::BoosterParameters::GbTree(tree_params))
            .learning_params(learning_params)
            .silent(true)
            .build()
            .unwrap();
        let mut booster = Booster::create(&[&dmat_train, &dmat_test], &params).unwrap();

        let num_rounds = 5;
        for i in 0..num_rounds {
            booster.update(&dmat_train, i).expect("update failed");
        }

        let preds = booster.predict_interactions(&dmat_test).unwrap();
        let num_samples = dmat_test.num_rows();
        let num_features = dmat_train.num_cols();
        assert_eq!(preds.shape(), &[num_samples, num_features + 1, num_features + 1]);
    }

    #[test]
    fn parse_eval_string() {
        let s = "[0]\ttrain-map@4-:0.5\ttrain-logloss:1.0\ttest-map@4-:0.25\ttest-logloss:0.75";
        let mut metrics = BTreeMap::new();

        let mut train_metrics = BTreeMap::new();
        train_metrics.insert("map@4-".to_owned(), 0.5);
        train_metrics.insert("logloss".to_owned(), 1.0);

        let mut test_metrics = BTreeMap::new();
        test_metrics.insert("map@4-".to_owned(), 0.25);
        test_metrics.insert("logloss".to_owned(), 0.75);

        metrics.insert("train".to_owned(), train_metrics);
        metrics.insert("test".to_owned(), test_metrics);
        assert_eq!(Booster::parse_eval_string(s, &["train", "test"]), metrics);
    }

    #[test]
    fn dump_model() {
        let dmat_train = DMatrix::load("xgboost-sys/xgboost/demo/data/agaricus.txt.train").unwrap();

        let tree_params = tree::TreeBoosterParametersBuilder::default()
            .max_depth(2)
            .eta(1.0)
            .build().unwrap();
        let learning_params = learning::LearningTaskParametersBuilder::default()
            .objective(learning::Objective::BinaryLogistic)
            .build().unwrap();
        let params = parameters::ParametersBuilder::default()
            .booster_params(parameters::booster::BoosterParameters::GbTree(tree_params))
            .learning_params(learning_params)
            .silent(true)
            .build().unwrap();
        let booster = Booster::train(&params, &dmat_train, 10, &[]).unwrap();

        let file = File::open("xgboost-sys/xgboost/demo/data/featmap.txt").expect("failed to open feature map file");
        let mut reader = BufReader::new(&file);
        let mut features: Vec<Feature> = Vec::new();
        for (i, line) in reader.lines().enumerate() {
            let line = line.unwrap();
            let parts: Vec<&str> = line.split('\t').collect();
            let feature_num: usize = parts.get(0).unwrap().parse().unwrap();
            let feature_name = parts.get(1).unwrap();
            let feature_type = parts.get(2).unwrap();
            let feature = Feature::from_type(*feature_name, feature_type).unwrap();
            assert_eq!(i, features.len());
            features.push(feature);
        }
    }
}
