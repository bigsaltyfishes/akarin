use alloc::{
    string::{String, ToString},
    vec::Vec,
};

pub struct Path {
    absolute: bool,
    location: Vec<String>,
}

impl Path {
    pub fn try_from<T>(s: T) -> Option<Self>
    where
        T: AsRef<str>,
    {
        let s = s.as_ref();
        let absolute = s.starts_with('/');
        let components: Vec<String> = s
            .split('/')
            .filter(|comp| !comp.is_empty())
            .map(|comp| comp.to_string())
            .collect();

        Some(Self {
            absolute,
            location: components,
        })
    }

    pub fn join<T>(&self, other: T) -> Self
    where
        T: AsRef<Path>,
    {
        let other_path = other.as_ref();
        let mut new_location = self.location.clone();
        new_location.extend_from_slice(&other_path.location);

        Self {
            absolute: self.absolute,
            location: new_location,
        }
    }

    pub fn is_absolute(&self) -> bool {
        self.absolute
    }

    pub fn components(&self) -> &Vec<String> {
        &self.location
    }
}

impl AsRef<Path> for Path {
    fn as_ref(&self) -> &Path {
        self
    }
}
