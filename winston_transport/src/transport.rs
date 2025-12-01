use crate::log_query::LogQuery;

pub trait Transport<L> {
    fn log(&self, info: L);

    fn log_batch(&self, logs: Vec<L>) {
        for log_info in logs {
            self.log(log_info);
        }
    }

    fn flush(&self) -> Result<(), String> {
        Ok(())
    }

    fn query(&self, _options: &LogQuery) -> Result<Vec<L>, String> {
        Ok(Vec::new())
    }
}
