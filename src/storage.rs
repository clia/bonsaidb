use async_trait::async_trait;
use std::{marker::PhantomData, path::Path, sync::Arc};

use crate::{
    connection::{Collection, Connection, Error},
    schema::{self, Database, Schema},
};

#[derive(Clone)]
pub struct Storage<DB> {
    sled: sled::Db,
    collections: Arc<Schema>,
    _schema: PhantomData<DB>,
    // views: Arc<Views>,
}

impl<DB> Storage<DB>
where
    DB: Database,
{
    pub fn open_local<P: AsRef<Path>>(path: P) -> Result<Self, sled::Error> {
        let mut collections = Schema::default();
        DB::define_collections(&mut collections);
        // let views = Views::default();
        // for collection in collections.collections.values() {
        //     // TODO Collect the views from the collections, which will allow us to expose storage.view::<Type>() directly without needing to navigate the Collection first
        // }

        sled::open(path).map(|sled| Self {
            sled,
            collections: Arc::new(collections),
            _schema: PhantomData::default(),
        })
    }
}

#[async_trait]
impl<DB> Connection for Storage<DB>
where
    DB: Database,
{
    fn collection<C: schema::Collection + 'static>(&self) -> Result<Collection<'_, Self, C>, Error>
    where
        Self: Sized,
    {
        if self.collections.contains::<C>() {
            Ok(Collection::new(self))
        } else {
            Err(Error::CollectionNotFound)
        }
    }

    async fn save<C: schema::Collection>(&self, doc: &schema::Document<C>) -> Result<(), Error> {
        todo!()
    }

    async fn update<C: schema::Collection>(
        &self,
        doc: &mut schema::Document<C>,
    ) -> Result<(), Error> {
        todo!()
    }
}
