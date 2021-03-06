use std::cell::UnsafeCell;
use std::rc::Rc;
use amy::Registrar;
use std::collections::HashSet;

/// Creates a throttler from which sub throttles may be created.
/// Note that all created throttle's have a lifetime tied to the
/// throttler. This invariant must be maintained or undefined
/// behaviour will occur.
pub struct Throttler {
    id: usize,
    fid: usize,
    last_ul: u64,
    last_dl: u64,
    dl_data: Rc<UnsafeCell<ThrottleData>>,
    ul_data: Rc<UnsafeCell<ThrottleData>>,
}

const URATE: usize = 15;

impl Throttler {
    /// Creates a new throttler and sets two timers on reg,
    /// one for updating the tokens, the other for flushing out
    /// blocked peers.
    pub fn new(dl_rate: usize, ul_rate: usize, max_tokens: usize, reg: &Registrar) -> Throttler {
        let id = reg.set_interval(URATE).unwrap();
        let fid = reg.set_interval(50).unwrap();
        let ut = ThrottleData::new(ul_rate, max_tokens);
        let dt = ThrottleData::new(dl_rate, max_tokens);
        Throttler {
            id,
            fid,
            last_ul: 0,
            last_dl: 0,
            ul_data: Rc::new(UnsafeCell::new(ut)),
            dl_data: Rc::new(UnsafeCell::new(dt)),
        }
    }

    pub fn update(&mut self) -> Option<(u64, u64)> {
        let ul = self.ul_data().add_tokens();
        let dl = self.dl_data().add_tokens();
        if ul != self.last_ul || dl != self.last_dl {
            self.last_ul = ul;
            self.last_dl = dl;
            Some((ul, dl))
        } else {
            None
        }
    }

    pub fn get_throttle(&self, id: usize) -> Throttle {
        Throttle {
            ul_data: self.ul_data.clone(),
            dl_data: self.dl_data.clone(),
            id,
        }
    }

    pub fn ul_rate(&mut self) -> usize {
        self.ul_data().rate
    }

    pub fn dl_rate(&mut self) -> usize {
        self.dl_data().rate
    }

    pub fn set_ul_rate(&mut self, rate: usize) {
        self.ul_data().rate = rate;
    }

    pub fn set_dl_rate(&mut self, rate: usize) {
        self.dl_data().rate = rate;
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn fid(&self) -> usize {
        self.fid
    }

    pub fn flush_ul(&mut self) -> Vec<usize> {
        self.ul_data().throttled.drain().collect()
    }

    pub fn flush_dl(&mut self) -> Vec<usize> {
        self.dl_data().throttled.drain().collect()
    }

    fn ul_data<'f>(&self) -> &'f mut ThrottleData {
        unsafe { self.ul_data.get().as_mut().unwrap() }
    }

    fn dl_data<'f>(&self) -> &'f mut ThrottleData {
        unsafe { self.dl_data.get().as_mut().unwrap() }
    }
}

struct ThrottleData {
    rate: usize,
    tokens: usize,
    max_tokens: usize,
    last_used: u64,
    throttled: HashSet<usize>,
}

/// Throttle mechanism based on the token bucket algorithm.
/// Expected to be called every millisecond, and operates on
/// a KB/s rate scale.
#[derive(Clone)]
pub struct Throttle {
    pub id: usize,
    ul_data: Rc<UnsafeCell<ThrottleData>>,
    dl_data: Rc<UnsafeCell<ThrottleData>>,
}

unsafe impl Send for Throttle {}

impl Throttle {
    pub fn new_sibling(&self, id: usize) -> Throttle {
        Throttle {
            ul_data: self.ul_data.clone(),
            dl_data: self.dl_data.clone(),
            id,
        }
    }

    pub fn get_bytes_dl(&mut self, amnt: usize) -> Result<(), ()> {
        let res = self.dl_data().get_tokens(amnt);
        if res.is_err() {
            self.dl_data().throttled.insert(self.id);
        }
        res
    }

    pub fn get_bytes_ul(&mut self, amnt: usize) -> Result<(), ()> {
        let res = self.ul_data().get_tokens(amnt);
        if res.is_err() {
            self.ul_data().throttled.insert(self.id);
        }
        res
    }

    pub fn ul_rate(&self) -> usize {
        self.ul_data().rate
    }

    pub fn dl_rate(&self) -> usize {
        self.dl_data().rate
    }

    // TODO: Make this an HTB
    pub fn set_ul_rate(&mut self, rate: usize) {
        self.ul_data().rate = rate;
    }

    pub fn set_dl_rate(&mut self, rate: usize) {
        self.dl_data().rate = rate;
    }


    pub fn restore_bytes_dl(&mut self, amnt: usize) {
        self.dl_data().restore_tokens(amnt);
    }

    pub fn restore_bytes_ul(&mut self, amnt: usize) {
        self.ul_data().restore_tokens(amnt);
    }

    fn ul_data(&self) -> &'static mut ThrottleData {
        unsafe { self.ul_data.get().as_mut().unwrap() }
    }

    fn dl_data(&self) -> &'static mut ThrottleData {
        unsafe { self.dl_data.get().as_mut().unwrap() }
    }
}

impl Drop for Throttle {
    fn drop(&mut self) {
        self.ul_data().throttled.remove(&self.id);
        self.dl_data().throttled.remove(&self.id);
    }
}

impl ThrottleData {
    /// Creates a new Throttle with the given rate and max token amount.
    fn new(rate: usize, max_tokens: usize) -> ThrottleData {
        ThrottleData {
            tokens: 0,
            rate,
            max_tokens,
            throttled: HashSet::new(),
            last_used: 0,
        }
    }

    /// Adds some amount of tokens back.
    fn restore_tokens(&mut self, amnt: usize) {
        self.last_used -= amnt as u64;
        self.tokens += amnt;
    }

    /// This method must be called every URATE milliseconds and returns
    /// (self.last_used) * 1000/URATE - the bits/s, clearing self.last_used
    fn add_tokens(&mut self) -> u64 {
        let drained = self.last_used as u64;
        self.last_used = 0;
        self.tokens += self.rate * URATE;
        if self.tokens >= self.max_tokens {
            self.tokens = self.max_tokens;
        }
        return drained / URATE as u64 * 1000;
    }

    /// Attempt to extract amnt tokens from the throttler.
    fn get_tokens(&mut self, amnt: usize) -> Result<(), ()> {
        if self.rate == 0 {
            self.last_used += amnt as u64;
            return Ok(());
        }
        if amnt > self.tokens {
            Err(())
        } else {
            self.last_used += amnt as u64;
            self.tokens -= amnt;
            Ok(())
        }
    }
}
