use crate::error::Error;

/// One row to append to the review log.
pub struct NewReview {
    pub model: String,
    pub task: String,
    pub what: String,
    pub duration: String,
    pub cost: String,
    pub score: String,
    pub reviewer: String,
}

pub fn write(_review: NewReview) -> Result<i32, Error> {
    Err(Error::new(2, "crew review write is not implemented yet"))
}

pub fn query(_harness: &str) -> Result<i32, Error> {
    Err(Error::new(2, "crew review query is not implemented yet"))
}

pub fn pick(_task: &str) -> Result<i32, Error> {
    Err(Error::new(2, "crew review pick is not implemented yet"))
}
