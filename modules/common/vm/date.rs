//! `Date`: time values, the civil calendar, and the text forms.

use super::*;

/// One millisecond, one second, one minute, one hour, and one day, in the
/// milliseconds a time value counts.
pub(super) const MS_PER_SECOND: f64 = 1000.0;

pub(super) const MS_PER_MINUTE: f64 = 60_000.0;

pub(super) const MS_PER_HOUR: f64 = 3_600_000.0;

pub(super) const MS_PER_DAY: f64 = 86_400_000.0;

/// The farthest a time value may lie from the epoch.
pub(super) const TIME_RANGE: f64 = 8.64e15;

/// The fields of a time value, in UTC.
#[derive(Clone, Copy)]
pub struct DateFields {
    pub year: i32,
    pub month: i32,
    pub date: i32,
    pub weekday: i32,
    pub hours: i32,
    pub minutes: i32,
    pub seconds: i32,
    pub milliseconds: i32,
}

/// Which text a date is asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DateForm {
    Full,
    Utc,
    Date,
    Time,
    Iso,
}

/// TimeClip: a time value within range, made an integer, or NaN.
pub fn time_clip(time: f64) -> f64 {
    if !time.is_finite() || value::floor(if time < 0.0 { -time } else { time }) > TIME_RANGE {
        return f64::NAN;
    }
    let integral = value::truncate(time);
    if integral == 0.0 {
        0.0
    } else {
        integral
    }
}

/// Floor division of whole numbers held as doubles: the arithmetic below
/// stays in f64 and i32, since a small target has no 64-bit division.
pub(super) fn floor_div(numerator: f64, denominator: f64) -> f64 {
    value::floor(numerator / denominator)
}

/// Days from the epoch to `day` of `month` (0..12) in `year`, proleptic
/// Gregorian.
pub(super) fn days_from_civil(year: f64, month: f64, day: f64) -> f64 {
    // Howard Hinnant's algorithm, with March as the year's first month.
    let year = if month <= 1.0 { year - 1.0 } else { year };
    let era = floor_div(year, 400.0);
    let year_of_era = year - era * 400.0;
    let month_of_era = (month + 10.0) - floor_div(month + 10.0, 12.0) * 12.0;
    let day_of_year = floor_div(153.0 * month_of_era + 2.0, 5.0) + day - 1.0;
    let day_of_era = year_of_era * 365.0 + floor_div(year_of_era, 4.0)
        - floor_div(year_of_era, 100.0)
        + day_of_year;
    era * 146_097.0 + day_of_era - 719_468.0
}

/// The year, month (0..12), and day (1..) a day count from the epoch names.
pub(super) fn civil_from_days(days: f64) -> (f64, f64, f64) {
    let shifted = days + 719_468.0;
    let era = floor_div(shifted, 146_097.0);
    let day_of_era = shifted - era * 146_097.0;
    let year_of_era = floor_div(
        day_of_era - floor_div(day_of_era, 1460.0) + floor_div(day_of_era, 36_524.0)
            - floor_div(day_of_era, 146_096.0),
        365.0,
    );
    let year = year_of_era + era * 400.0;
    let day_of_year = day_of_era
        - (365.0 * year_of_era + floor_div(year_of_era, 4.0) - floor_div(year_of_era, 100.0));
    let month_of_era = floor_div(5.0 * day_of_year + 2.0, 153.0);
    let day = day_of_year - floor_div(153.0 * month_of_era + 2.0, 5.0) + 1.0;
    let month = if month_of_era < 10.0 {
        month_of_era + 2.0
    } else {
        month_of_era - 10.0
    };
    let year = if month <= 1.0 { year + 1.0 } else { year };
    (year, month, day)
}

/// The fields a finite time value has.
pub fn date_fields(time: f64) -> DateFields {
    let day = value::floor(time / MS_PER_DAY);
    let within = time - day * MS_PER_DAY;
    let (year, month, date) = civil_from_days(day);
    let weekday = (day + 4.0) - floor_div(day + 4.0, 7.0) * 7.0;
    DateFields {
        year: year as i32,
        month: month as i32,
        date: date as i32,
        weekday: weekday as i32,
        hours: value::floor(within / MS_PER_HOUR) as i32,
        minutes: (value::floor(within / MS_PER_MINUTE) as i32) % 60,
        seconds: (value::floor(within / MS_PER_SECOND) as i32) % 60,
        milliseconds: (within as i32) % 1000,
    }
}

/// MakeDate over MakeDay and MakeTime, every part a finite number already.
pub(super) fn make_date(
    year: f64,
    month: f64,
    date: f64,
    hours: f64,
    minutes: f64,
    seconds: f64,
    ms: f64,
) -> f64 {
    let parts = [year, month, date, hours, minutes, seconds, ms];
    if parts.iter().any(|part| !part.is_finite()) {
        return f64::NAN;
    }
    let year = value::truncate(year);
    let month = value::truncate(month);
    let whole_year = year + value::floor(month / 12.0);
    let month_in_year = month - value::floor(month / 12.0) * 12.0;
    if whole_year.abs() > 400_000.0 {
        return f64::NAN;
    }
    let days = days_from_civil(whole_year, month_in_year, 1.0) + value::truncate(date) - 1.0;
    let time = value::truncate(hours) * MS_PER_HOUR
        + value::truncate(minutes) * MS_PER_MINUTE
        + value::truncate(seconds) * MS_PER_SECOND
        + value::truncate(ms);
    days * MS_PER_DAY + time
}

// Fixed-size arrays, not slices: a table of references would carry
// relocations a loaded module cannot bear.
pub(super) const DAY_NAMES: [[u8; 3]; 7] = [
    *b"Sun", *b"Mon", *b"Tue", *b"Wed", *b"Thu", *b"Fri", *b"Sat",
];

pub(super) const MONTH_NAMES: [[u8; 3]; 12] = [
    *b"Jan", *b"Feb", *b"Mar", *b"Apr", *b"May", *b"Jun", *b"Jul", *b"Aug", *b"Sep", *b"Oct",
    *b"Nov", *b"Dec",
];

pub(super) fn put_digits(out: &mut [u8], at: &mut usize, value: i32, width: usize) {
    let mut digits = [0u8; 20];
    let mut count = 0usize;
    let mut remaining = value.unsigned_abs();
    loop {
        digits[count] = b'0' + (remaining % 10) as u8;
        count += 1;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    while count < width {
        digits[count] = b'0';
        count += 1;
    }
    while count > 0 {
        count -= 1;
        if *at < out.len() {
            out[*at] = digits[count];
            *at += 1;
        }
    }
}

pub(super) fn put_text(out: &mut [u8], at: &mut usize, text: &[u8]) {
    for &byte in text {
        if *at < out.len() {
            out[*at] = byte;
            *at += 1;
        }
    }
}

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// "Now": the wall clock a deployment granted, or — absent one — a
    /// logical instant, each reading one tick after the last, kept on the
    /// global so it survives the machine's rebuilds. Deterministic,
    /// monotonic, and telling nothing of the world outside but how often it
    /// was asked.
    pub(super) fn date_now(&mut self) -> f64 {
        // A granted clock is the real one, supplied at the task boundary by
        // the host. Every reading of "now" goes through here — `Date.now()`,
        // `new Date()` and `Date()` alike — so they cannot disagree about
        // what time it is, which they would if only one of them were wired.
        if let Ok(wall) = self.ascii_key(b"\0wall") {
            if let Some(descriptor) = object::get_own_property(self.heap, self.realm.global, wall)
                .ok()
                .flatten()
            {
                if matches!(descriptor.value.tag(), Tag::Number) {
                    let millis = descriptor.value.as_number();
                    if millis > 0.0 {
                        return millis;
                    }
                }
            }
        }
        let Ok(key) = self.ascii_key(b"\0clock") else {
            return 0.0;
        };
        let held = object::get_own_property(self.heap, self.realm.global, key)
            .ok()
            .flatten()
            .map_or(0.0, |descriptor| {
                if matches!(descriptor.value.tag(), Tag::Number) {
                    descriptor.value.as_number()
                } else {
                    0.0
                }
            });
        let next = held + 1.0;
        let _ = object::define_own_property(
            self.heap,
            self.realm.global,
            key,
            Descriptor::data(Value::number(next), attribute::WRITABLE),
        );
        next
    }

    /// The time value a date holds, or a TypeError for anything else.
    pub(super) fn date_time_of(&mut self, this: Value) -> Result<f64, Completion> {
        if this.is_object() {
            let key = self.ascii_key(b"\0time")?;
            let held = object::get_own_property(self.heap, this.as_handle(), key)
                .map_err(|_| Completion::MALFORMED)?;
            if let Some(descriptor) = held {
                if matches!(descriptor.value.tag(), Tag::Number) {
                    return Ok(descriptor.value.as_number());
                }
            }
        }
        Err(self.throw_type_error())
    }

    pub(super) fn date_set_time(&mut self, this: Value, time: f64) -> Result<(), Completion> {
        let key = self.ascii_key(b"\0time")?;
        object::define_own_property(
            self.heap,
            this.as_handle(),
            key,
            Descriptor::data(Value::number(time), attribute::WRITABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(())
    }

    /// `new Date(...)`: no argument is now, one is a time value — a date's
    /// own, a string to parse, or a number — and more are components.
    pub(super) fn construct_date(&mut self, arguments: &[Value]) -> Result<Value, Completion> {
        let time = match arguments.len() {
            0 => self.date_now(),
            1 => {
                let only = arguments[0];
                let own = if only.is_object() {
                    let key = self.ascii_key(b"\0time")?;
                    object::get_own_property(self.heap, only.as_handle(), key)
                        .map_err(|_| Completion::MALFORMED)?
                        .map(|descriptor| descriptor.value)
                } else {
                    None
                };
                match own {
                    Some(held) if matches!(held.tag(), Tag::Number) => held.as_number(),
                    _ => {
                        let primitive = self.coerce_to_primitive(only, Hint::Default)?;
                        if matches!(primitive.tag(), Tag::String) {
                            self.date_parse(primitive.as_handle())?
                        } else {
                            time_clip(self.coerce_to_number(primitive)?)
                        }
                    }
                }
            }
            _ => {
                let made = self.date_from_components(arguments)?;
                made.as_number()
            }
        };
        let made = self.new_instance_of(self.realm.date_prototype)?;
        let value = Value::object(made);
        self.date_set_time(value, time)?;
        Ok(value)
    }

    /// A time value from year, month, and optional date, hours, minutes,
    /// seconds, and milliseconds — `Date.UTC`, and `new Date` with more
    /// than one argument. A two-digit year lies in the twentieth century.
    pub(super) fn date_from_components(
        &mut self,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let mut parts = [f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        for (index, slot) in parts.iter_mut().enumerate() {
            if let Some(&argument) = arguments.get(index) {
                *slot = self.coerce_to_number(argument)?;
            }
        }
        if arguments.is_empty() {
            return Ok(Value::number(f64::NAN));
        }
        if parts[0].is_finite() {
            let whole = value::truncate(parts[0]);
            if (0.0..=99.0).contains(&whole) {
                parts[0] = 1900.0 + whole;
            }
        }
        let time = make_date(
            parts[0], parts[1], parts[2], parts[3], parts[4], parts[5], parts[6],
        );
        Ok(Value::number(time_clip(time)))
    }

    /// The text of a time value: the ISO form, the `toString` form and its
    /// date and time halves, or the UTC form.
    pub(super) fn date_to_string(
        &mut self,
        time: f64,
        form: DateForm,
    ) -> Result<Value, Completion> {
        if time.is_nan() {
            return self.ascii_string(b"Invalid Date");
        }
        let fields = date_fields(time);
        let mut out = [0u8; 64];
        let mut at = 0usize;
        match form {
            DateForm::Iso => {
                let year = fields.year;
                if !(0..=9999).contains(&year) {
                    put_text(&mut out, &mut at, if year < 0 { b"-" } else { b"+" });
                    put_digits(&mut out, &mut at, year, 6);
                } else {
                    put_digits(&mut out, &mut at, year, 4);
                }
                put_text(&mut out, &mut at, b"-");
                put_digits(&mut out, &mut at, fields.month + 1, 2);
                put_text(&mut out, &mut at, b"-");
                put_digits(&mut out, &mut at, fields.date, 2);
                put_text(&mut out, &mut at, b"T");
                put_digits(&mut out, &mut at, fields.hours, 2);
                put_text(&mut out, &mut at, b":");
                put_digits(&mut out, &mut at, fields.minutes, 2);
                put_text(&mut out, &mut at, b":");
                put_digits(&mut out, &mut at, fields.seconds, 2);
                put_text(&mut out, &mut at, b".");
                put_digits(&mut out, &mut at, fields.milliseconds, 3);
                put_text(&mut out, &mut at, b"Z");
            }
            DateForm::Utc => {
                put_text(&mut out, &mut at, &DAY_NAMES[fields.weekday as usize % 7]);
                put_text(&mut out, &mut at, b", ");
                put_digits(&mut out, &mut at, fields.date, 2);
                put_text(&mut out, &mut at, b" ");
                put_text(&mut out, &mut at, &MONTH_NAMES[fields.month as usize % 12]);
                put_text(&mut out, &mut at, b" ");
                self.put_year(&mut out, &mut at, fields.year);
                put_text(&mut out, &mut at, b" ");
                self.put_clock(&mut out, &mut at, &fields);
                put_text(&mut out, &mut at, b" GMT");
            }
            DateForm::Full | DateForm::Date | DateForm::Time => {
                if form != DateForm::Time {
                    put_text(&mut out, &mut at, &DAY_NAMES[fields.weekday as usize % 7]);
                    put_text(&mut out, &mut at, b" ");
                    put_text(&mut out, &mut at, &MONTH_NAMES[fields.month as usize % 12]);
                    put_text(&mut out, &mut at, b" ");
                    put_digits(&mut out, &mut at, fields.date, 2);
                    put_text(&mut out, &mut at, b" ");
                    self.put_year(&mut out, &mut at, fields.year);
                }
                if form == DateForm::Full {
                    put_text(&mut out, &mut at, b" ");
                }
                if form != DateForm::Date {
                    self.put_clock(&mut out, &mut at, &fields);
                    put_text(&mut out, &mut at, b" GMT+0000 (Coordinated Universal Time)");
                }
            }
        }
        self.ascii_string(&out[..at])
    }

    pub(super) fn put_year(&self, out: &mut [u8], at: &mut usize, year: i32) {
        if year < 0 {
            put_text(out, at, b"-");
            put_digits(out, at, -year, 4);
        } else {
            put_digits(out, at, year, 4);
        }
    }

    pub(super) fn put_clock(&self, out: &mut [u8], at: &mut usize, fields: &DateFields) {
        put_digits(out, at, fields.hours, 2);
        put_text(out, at, b":");
        put_digits(out, at, fields.minutes, 2);
        put_text(out, at, b":");
        put_digits(out, at, fields.seconds, 2);
    }

    /// The time value a string denotes in the ISO date-time form —
    /// `YYYY-MM-DDTHH:mm:ss.sssZ`, with the time, the seconds, the
    /// milliseconds, and the offset optional, and an expanded year with a
    /// sign — or NaN for anything else.
    pub(super) fn date_parse(&mut self, text: Handle) -> Result<f64, Completion> {
        let length = string::length(self.heap, text).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let mut units = [0u16; 40];
        if length as usize > units.len() {
            return Ok(f64::NAN);
        }
        string::copy_units(self.heap, text, &mut units[..length as usize])
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let units = &units[..length as usize];
        let mut at = 0usize;
        let digits = |units: &[u16], at: &mut usize, count: usize| -> Option<i32> {
            let mut value = 0i32;
            for _ in 0..count {
                let unit = *units.get(*at)?;
                if !(0x30..=0x39).contains(&unit) {
                    return None;
                }
                value = value * 10 + i32::from(unit - 0x30);
                *at += 1;
            }
            Some(value)
        };
        let eat = |units: &[u16], at: &mut usize, unit: u16| -> bool {
            if units.get(*at).copied() == Some(unit) {
                *at += 1;
                true
            } else {
                false
            }
        };
        let year = match units.first().copied() {
            Some(0x2B) => {
                at += 1;
                digits(units, &mut at, 6)
            }
            Some(0x2D) => {
                at += 1;
                digits(units, &mut at, 6).map(|year| -year)
            }
            _ => digits(units, &mut at, 4),
        };
        let Some(year) = year else {
            return Ok(f64::NAN);
        };
        let mut month = 1i32;
        let mut day = 1i32;
        if eat(units, &mut at, 0x2D) {
            let Some(parsed) = digits(units, &mut at, 2) else {
                return Ok(f64::NAN);
            };
            month = parsed;
            if eat(units, &mut at, 0x2D) {
                let Some(parsed) = digits(units, &mut at, 2) else {
                    return Ok(f64::NAN);
                };
                day = parsed;
            }
        }
        let (mut hours, mut minutes, mut seconds, mut ms) = (0i32, 0i32, 0i32, 0i32);
        let mut offset = 0i32;
        if eat(units, &mut at, 0x54) {
            let (Some(h), true, Some(m)) = (
                digits(units, &mut at, 2),
                eat(units, &mut at, 0x3A),
                digits(units, &mut at, 2),
            ) else {
                return Ok(f64::NAN);
            };
            hours = h;
            minutes = m;
            if eat(units, &mut at, 0x3A) {
                let Some(s) = digits(units, &mut at, 2) else {
                    return Ok(f64::NAN);
                };
                seconds = s;
                if eat(units, &mut at, 0x2E) {
                    let Some(fraction) = digits(units, &mut at, 3) else {
                        return Ok(f64::NAN);
                    };
                    ms = fraction;
                }
            }
            if !eat(units, &mut at, 0x5A) {
                if let Some(sign @ (0x2B | 0x2D)) = units.get(at).copied() {
                    at += 1;
                    let (Some(oh), true, Some(om)) = (
                        digits(units, &mut at, 2),
                        eat(units, &mut at, 0x3A),
                        digits(units, &mut at, 2),
                    ) else {
                        return Ok(f64::NAN);
                    };
                    offset = (oh * 60 + om) * if sign == 0x2D { -1 } else { 1 };
                }
            }
        }
        if at != units.len() {
            return Ok(f64::NAN);
        }
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hours > 24
            || minutes > 59
            || seconds > 59
            || (hours == 24 && (minutes > 0 || seconds > 0 || ms > 0))
        {
            return Ok(f64::NAN);
        }
        let time = make_date(
            f64::from(year),
            f64::from(month - 1),
            f64::from(day),
            f64::from(hours),
            f64::from(minutes),
            f64::from(seconds),
            f64::from(ms),
        ) - f64::from(offset) * MS_PER_MINUTE;
        Ok(time_clip(time))
    }
}
