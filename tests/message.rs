mod common;

use common::*;

use std::{borrow::Cow, io::BufWriter, sync::Arc};

use anyhow::Result;
use itertools::Itertools;
use memmap::Mmap;
use rayon::prelude::*;
use tempfile::tempfile;

#[test]
fn smoke() -> Result<()> {
    let mapped = map_mcap("tests/references/OneMessage.mcap")?;
    let messages = mcap::MessageStream::new(&mapped)?.collect::<mcap::McapResult<Vec<_>>>()?;

    assert_eq!(messages.len(), 1);

    let expected = mcap::Message {
        channel: Arc::new(mcap::Channel {
            schema: Some(Arc::new(mcap::Schema {
                name: String::from("Example"),
                encoding: String::from("c"),
                data: Cow::Borrowed(&[4, 5, 6]),
            })),
            topic: String::from("example"),
            message_encoding: String::from("a"),
            metadata: [(String::from("foo"), String::from("bar"))].into(),
        }),
        sequence: 10,
        log_time: 2,
        publish_time: 1,
        data: Cow::Borrowed(&[1, 2, 3]),
    };

    assert_eq!(messages[0], expected);

    Ok(())
}

#[test]
fn round_trip() -> Result<()> {
    let mapped = map_mcap("tests/references/OneMessage.mcap")?;
    let messages = mcap::MessageStream::new(&mapped)?;

    let mut tmp = tempfile()?;
    let mut writer = mcap::Writer::new(BufWriter::new(&mut tmp))?;

    for m in messages {
        writer.write(&m?)?;
    }
    drop(writer);

    let ours = unsafe { Mmap::map(&tmp) }?;
    let summary = mcap::Summary::read(&ours)?.unwrap();

    let schema = Arc::new(mcap::Schema {
        name: String::from("Example"),
        encoding: String::from("c"),
        data: Cow::Borrowed(&[4, 5, 6]),
    });

    let channel = Arc::new(mcap::Channel {
        schema: Some(schema.clone()),
        topic: String::from("example"),
        message_encoding: String::from("a"),
        metadata: [(String::from("foo"), String::from("bar"))].into(),
    });

    let expected_summary = mcap::Summary {
        stats: Some(mcap::records::Statistics {
            message_count: 1,
            schema_count: 1,
            channel_count: 1,
            chunk_count: 1,
            message_start_time: 2,
            message_end_time: 2,
            channel_message_counts: [(0, 1)].into(),
            ..Default::default()
        }),
        channels: [(0, channel.clone())].into(),
        schemas: [(1, schema.clone())].into(),
        ..Default::default()
    };
    // Don't assert the chunk indexes - their size is at the whim of compressors.
    assert_eq!(summary.stats, expected_summary.stats);
    assert_eq!(summary.channels, expected_summary.channels);
    assert_eq!(summary.schemas, expected_summary.schemas);
    assert_eq!(
        summary.attachment_indexes,
        expected_summary.attachment_indexes
    );
    assert_eq!(summary.metadata_indexes, expected_summary.metadata_indexes);

    let expected = mcap::Message {
        channel,
        sequence: 10,
        log_time: 2,
        publish_time: 1,
        data: Cow::Borrowed(&[1, 2, 3]),
    };

    assert_eq!(
        mcap::MessageStream::new(&ours)?.collect::<mcap::McapResult<Vec<_>>>()?,
        &[expected]
    );

    Ok(())
}

#[test]
fn demo_round_trip() -> Result<()> {
    let mapped = map_mcap("tests/references/demo.mcap")?;

    let messages = mcap::MessageStream::new(&mapped)?;

    let mut tmp = tempfile()?;
    let mut writer = mcap::Writer::new(BufWriter::new(&mut tmp))?;

    for m in messages {
        // IRL, we'd add channels, then write messages to known channels,
        // which skips having to re-hash the channel and its schema each time.
        // But since here we'd need to do the same anyways...
        writer.write(&m?)?;
    }
    drop(writer);

    let ours = unsafe { Mmap::map(&tmp) }?;

    // Compare the message stream of our MCAP to the reference one.
    for (theirs, ours) in
        mcap::MessageStream::new(&mapped)?.zip_eq(mcap::MessageStream::new(&ours)?)
    {
        assert_eq!(ours?, theirs?)
    }

    // Verify the summary and its connectivity.

    let summary = mcap::Summary::read(&ours)?.unwrap();
    assert!(summary.attachment_indexes.is_empty());
    assert!(summary.metadata_indexes.is_empty());

    // EZ mode: Streamed chunks should match up with a file-level message stream.
    for (whole, by_chunk) in mcap::MessageStream::new(&ours)?.zip_eq(
        summary
            .chunk_indexes
            .iter()
            .map(|ci| summary.stream_chunk(&ours, ci).unwrap())
            .flatten(),
    ) {
        assert_eq!(whole?, by_chunk?);
    }

    // Hard mode: randomly access every message in the MCAP.
    // Yes, this is dumb and O(n^2).
    let mut messages = Vec::new();

    for ci in &summary.chunk_indexes {
        let mut offsets_and_messages = summary
            .read_message_indexes(&ours, ci)
            .unwrap()
            // At least parallelize the dumb.
            .into_par_iter()
            .map(|(_k, v)| v)
            .flatten()
            .map(|e| (e.offset, summary.seek_message(&ours, ci, &e).unwrap()))
            .collect::<Vec<(u64, mcap::Message)>>();

        offsets_and_messages.sort_unstable_by_key(|im| im.0);

        for om in offsets_and_messages {
            messages.push(om.1);
        }
    }

    for (streamed, seeked) in mcap::MessageStream::new(&ours)?.zip_eq(messages.into_iter()) {
        assert_eq!(streamed?, seeked);
    }

    Ok(())
}

#[test]
fn demo_random_chunk_access() -> Result<()> {
    let mapped = map_mcap("tests/references/demo.mcap")?;

    let summary = mcap::Summary::read(&mapped)?.unwrap();

    // Random access of the second chunk should match the stream of the whole file.
    let messages_in_first_chunk: usize = summary
        .read_message_indexes(&mapped, &summary.chunk_indexes[0])?
        .values()
        .map(|entries| entries.len())
        .sum();
    let messages_in_second_chunk: usize = summary
        .read_message_indexes(&mapped, &summary.chunk_indexes[1])?
        .values()
        .map(|entries| entries.len())
        .sum();

    for (whole, random) in mcap::MessageStream::new(&mapped)?
        .skip(messages_in_first_chunk)
        .take(messages_in_second_chunk)
        .zip_eq(summary.stream_chunk(&mapped, &summary.chunk_indexes[1])?)
    {
        assert_eq!(whole?, random?);
    }

    // Let's poke around the message indexes
    let mut index_entries = summary
        .read_message_indexes(&mapped, &summary.chunk_indexes[1])?
        .values()
        .flatten()
        .copied()
        .collect::<Vec<mcap::records::MessageIndexEntry>>();

    index_entries.sort_unstable_by_key(|e| e.offset);

    // Do a big dumb n^2 seek of each message (dear god, don't ever actually do this)
    for (entry, message) in index_entries
        .iter()
        .zip_eq(summary.stream_chunk(&mapped, &summary.chunk_indexes[1])?)
    {
        let seeked = summary.seek_message(&mapped, &summary.chunk_indexes[1], entry)?;
        assert_eq!(seeked, message?);
    }

    Ok(())
}