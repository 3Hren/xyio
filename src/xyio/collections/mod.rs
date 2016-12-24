pub struct PipelineVec {
    buf: Vec<Vec<u8>>,
    cur: usize,
    end: usize,
}

impl PipelineVec {
    pub fn new(size: usize) -> Self {
        PipelineVec {
            buf: vec![vec![]; size],
            cur: 0,
            end: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn new() {
        let vec = PipelineVec::new(10);
        assert_eq!(0, vec.cur);
        assert_eq!(0, vec.end);

        assert_eq!(10, vec.len());
    }

    #[test]
    fn push() {
        let mut vec = PipelineVec::new(10);
        vec.push(vec![]);

        assert_eq!(0, vec.cur);
        assert_eq!(1, vec.end);

    }
}
