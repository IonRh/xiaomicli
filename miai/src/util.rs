use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};

pub fn random_id(len: usize) -> String {
    Alphanumeric.sample_string(&mut rng(), len)
}
