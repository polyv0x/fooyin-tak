// SPDX-License-Identifier: GPL-3.0-or-later
// fooyin 0.13.1's audioformat.h needs <vector> to be included first.
#include <vector>

#include <core/engine/audioloader.h>
#include <core/engine/inputplugin.h>
#include <core/plugins/coreplugin.h>
#include <core/plugins/plugin.h>

#include <QBuffer>
#include <QCoreApplication>
#include <QFile>
#include <QPluginLoader>
#include <QtEndian>

#include <cstring>
#include <functional>
#include <iostream>
#include <limits>
#include <stdexcept>

namespace
{
void check(bool condition, const char *message)
{
    if(!condition)
    {
        throw std::runtime_error(message);
    }
}

QByteArray load(const QString &path)
{
    QFile file{path};
    check(file.open(QIODevice::ReadOnly), "could not open test fixture");
    return file.readAll();
}

void compare(const Fooyin::AudioBuffer &buffer, const QByteArray &expected, size_t offset)
{
    check(buffer.isValid(), "invalid PCM buffer");
    const auto bytes = buffer.constData();
    check(offset + bytes.size() <= static_cast<size_t>(expected.size()), "too many output samples");
    check(bytes.size() % 4 == 0, "unaligned S32 buffer");
    for(size_t index = 0; index < bytes.size(); index += 4)
    {
        int32_t actual{};
        std::memcpy(&actual, bytes.data() + index, 4);
        const auto reference = qFromLittleEndian<int32_t>(expected.constData() + offset + index);
        check(actual == reference, "PCM mismatch");
    }
}

// Exercise short reads as well as a normal in-memory archive-like source.
class ShortReadBuffer final : public QBuffer
{
  protected:
    qint64 readData(char *data, qint64 size) override { return QBuffer::readData(data, std::min(size, qint64{7})); }
};

// A synchronous pipe-style source with no seek implementation. Small reads
// ensure the decoder cannot rely on a whole header or frame arriving at once.
class SequentialInput final : public QIODevice
{
  public:
    explicit SequentialInput(QByteArray data) : m_data{std::move(data)}
    {
        check(open(QIODevice::ReadOnly | QIODevice::Unbuffered), "could not open sequential source");
    }
    bool isSequential() const override { return true; }
    bool atEnd() const override { return m_offset == m_data.size(); }
    std::function<void()> beforeRead;

  protected:
    qint64 readData(char *data, qint64 size) override
    {
        if(beforeRead)
        {
            beforeRead();
        }
        const auto count = std::min({size, qint64{7}, qint64{m_data.size() - m_offset}});
        std::memcpy(data, m_data.constData() + m_offset, static_cast<size_t>(count));
        m_offset += count;
        return count;
    }
    qint64 writeData(const char *, qint64) override { return -1; }

  private:
    QByteArray m_data;
    qsizetype m_offset{0};
};

class FailingSeekBuffer final : public QBuffer
{
  public:
    bool failSeek{false};
    bool seek(qint64 position) override { return !failSeek && QBuffer::seek(position); }
};

void verifyDecoder(const Fooyin::InputCreator &creator, const QString &directory, const QString &name,
                   bool deviceSource)
{
    const QString path = directory + '/' + name + QStringLiteral(".tak");
    const QByteArray expected = load(directory + '/' + name + QStringLiteral(".s32le"));
    ShortReadBuffer input;
    input.setData(load(path));
    check(input.open(QIODevice::ReadOnly | QIODevice::Unbuffered), "could not open input buffer");
    Fooyin::AudioSource source;
    // A nonexistent filename proves supplied QIODevices are used, not reopened.
    source.filepath = deviceSource ? QStringLiteral("archive://does-not-exist/track.tak") : path;
    source.device = deviceSource ? &input : nullptr;
    auto decoder = creator.decoder();
    const Fooyin::Track track;
    check(decoder->extensions().contains(QStringLiteral("tak")), "TAK extension missing");
    check(decoder->preferredExtensions().contains(QStringLiteral("tak")), "decoder is not preferred for TAK");
    const auto format = decoder->init(source, track, Fooyin::AudioDecoder::None);
    check(format && format->isValid(), "plugin init failed");
    check(format->sampleFormat() == Fooyin::SampleFormat::S32, "wrong output format");
    check(decoder->isSeekable(), "seeking disabled unexpectedly");
    const auto stride = static_cast<size_t>(format->bytesPerFrame());
    size_t offset = 0;
    while(true)
    {
        auto result = decoder->readAudio(413 * stride + 1);
        if(result.status == Fooyin::AudioDecoder::ReadStatus::EndOfStream)
        {
            break;
        }
        if(result.status == Fooyin::AudioDecoder::ReadStatus::Error)
        {
            throw std::runtime_error(result.error.toStdString());
        }
        check(result.status == Fooyin::AudioDecoder::ReadStatus::DecodedAudio, "unexpected read status");
        check(result.buffer.startTime() == (offset / stride) * 1000 / format->sampleRate(), "incorrect timestamp");
        check(static_cast<size_t>(result.buffer.byteCount()) <= 413 * stride + 1, "exceeded read size");
        compare(result.buffer, expected, offset);
        offset += result.buffer.byteCount();
    }
    check(offset == static_cast<size_t>(expected.size()), "lost final samples");
    for(uint64_t milliseconds : {0ULL, 13ULL, 1044ULL, 1045ULL, 1046ULL, 1ULL})
    {
        decoder->seek(milliseconds);
        const auto sample = std::min<uint64_t>(milliseconds * format->sampleRate() / 1000, expected.size() / stride);
        const auto result = decoder->readAudio(37 * stride);
        if(sample == static_cast<uint64_t>(expected.size()) / stride)
        {
            check(result.status == Fooyin::AudioDecoder::ReadStatus::EndOfStream, "EOF seek returned audio");
        }
        else
        {
            check(result.status == Fooyin::AudioDecoder::ReadStatus::DecodedAudio, "seek did not return audio");
            check(result.buffer.startTime() == sample * 1000 / format->sampleRate(), "wrong timestamp after seek");
            compare(result.buffer, expected, sample * stride);
        }
    }
    decoder->seek(std::numeric_limits<uint64_t>::max());
    check(decoder->readAudio(4096).status == Fooyin::AudioDecoder::ReadStatus::EndOfStream, "large seek overflowed");
    decoder->seek(0);
    compare(decoder->readBuffer(37 * stride), expected, 0);
    decoder->stop();
    decoder->stop();
    check(!decoder->isSeekable(), "stopped decoder is seekable");
    check(input.isOpen(), "plugin closed borrowed input");
    check(decoder->init(source, track, Fooyin::AudioDecoder::NoSeeking).has_value(), "reinitialisation failed");
    check(!decoder->isSeekable(), "NoSeeking ignored");
    decoder->seek(13);
    compare(decoder->readBuffer(37 * stride), expected, 0);
    decoder->requestAbort();
    check(decoder->readAudio(4096).status == Fooyin::AudioDecoder::ReadStatus::Error, "abort ignored");
}

void verifyErrors(const Fooyin::InputCreator &creator, const QString &directory)
{
    QByteArray data = load(directory + QStringLiteral("/mono8.tak"));
    // Corrupt the first audio header's checksum, leaving metadata intact.
    size_t start = 4;
    while((static_cast<unsigned char>(data.at(start)) & 0x7f) != 0)
    {
        const auto *p = reinterpret_cast<const unsigned char *>(data.constData() + start + 1);
        start += 4 + p[0] + (size_t{p[1]} << 8) + (size_t{p[2]} << 16);
    }
    start += 4;
    data[start + 5] = static_cast<char>(data.at(start + 5) ^ 1);
    QBuffer input{&data};
    check(input.open(QIODevice::ReadOnly), "could not open corrupt fixture");
    Fooyin::AudioSource source;
    source.device = &input;
    auto decoder = creator.decoder();
    const Fooyin::Track track;
    check(decoder->init(source, track, Fooyin::AudioDecoder::None).has_value(), "valid header rejected");
    const auto result = decoder->readAudio(4096);
    check(result.status == Fooyin::AudioDecoder::ReadStatus::Error, "corrupt data treated as EOF/audio");
    check(!result.error.isEmpty(), "missing corruption diagnostic");
    check(decoder->readAudio(4096).status == Fooyin::AudioDecoder::ReadStatus::Error, "error was not latched");
    decoder->stop();
    input.close();
    data[0] = 'X';
    input.open(QIODevice::ReadOnly);
    check(!decoder->init(source, track, Fooyin::AudioDecoder::None).has_value(), "invalid header accepted");
    check(!decoder->isSeekable(), "failed init retained old decoder");
}

void verifyReader(const Fooyin::InputCreator &creator, const QString &directory, bool deviceSource)
{
    const QString path = directory + QStringLiteral("/stereo16.tak");
    const auto pcm = load(directory + QStringLiteral("/stereo16.s32le"));
    ShortReadBuffer input;
    input.setData(load(path));
    check(input.open(QIODevice::ReadOnly | QIODevice::Unbuffered), "could not open metadata source");
    Fooyin::AudioSource source;
    source.filepath = deviceSource ? QStringLiteral("archive://missing/album.tak") : path;
    source.device = deviceSource ? &input : nullptr;
    auto reader = creator.reader();
    Fooyin::Track track{path};
    track.setTitle(QStringLiteral("CUE title"));
    track.setArtists({QStringLiteral("CUE artist")});
    check(!reader->readTrack(source, track), "uninitialised reader succeeded");
    check(reader->preferredExtensions().contains(QStringLiteral("tak")), "native TAK duration is not preferred");
    check(reader->init(source) && reader->readTrack(source, track), "TAK metadata read failed");
    check(track.sampleFrames() == static_cast<uint64_t>(pcm.size()) / 8, "incorrect total sample count");
    check(track.duration() == 2094, "missing or incorrect TAK duration");
    check(track.sampleRate() == 8000 && track.channels() == 2 && track.bitDepth() == 16,
          "incorrect TAK audio properties");
    check(track.fileSize() == static_cast<uint64_t>(input.size()) && track.bitrate() > 0,
          "missing TAK file properties");
    check(track.codec() == QStringLiteral("TAK") && track.encoding() == QStringLiteral("Lossless"),
          "incorrect codec metadata");
    check(track.title() == QStringLiteral("CUE title") && track.artists() == QStringList{QStringLiteral("CUE artist")},
          "technical reader overwrote tags");
    check(input.isOpen(), "metadata reader closed borrowed input");

    QByteArray invalid{"not a TAK file"};
    QBuffer bad{&invalid};
    check(bad.open(QIODevice::ReadOnly), "could not open invalid metadata input");
    source.device = &bad;
    check(!reader->init(source), "metadata reader accepted invalid TAK");
    check(!reader->readTrack(source, track), "failed metadata init retained previous properties");
}

void verifyCueScan(const Fooyin::InputCreator &creator, const QString &directory)
{
    Fooyin::AudioLoader loader;
    loader.addReader(QStringLiteral("TAK"), creator.reader);
    loader.addDecoder(QStringLiteral("TAK"), creator.decoder);
    Fooyin::Track image{directory + QStringLiteral("/stereo16.tak")};
    check(loader.readTrackMetadata(image), "registry could not obtain TAK image duration");

    // A final CUE track has no following INDEX: fooyin derives its duration
    // from the image metadata, then scans that segment with NoSeeking set.
    Fooyin::Track last{image};
    last.setCuePath(directory + QStringLiteral("/album.cue"));
    last.setOffset(1000);
    check(image.duration() > last.offset(), "final CUE track has unknown duration");
    last.setDuration(image.duration() - last.offset());
    check(last.duration() == 1094, "incorrect final CUE track duration");
    auto loaded =
        loader.loadDecoderForTrack(last, Fooyin::AudioDecoder::NoSeeking | Fooyin::AudioDecoder::NoInfiniteLooping);
    check(loaded.decoder != nullptr && loaded.format.has_value(), "could not load CUE waveform decoder");
    loaded.decoder->start();
    loaded.decoder->seek(last.offset());
    const auto expected = load(directory + QStringLiteral("/stereo16.s32le"));
    const auto stride = static_cast<size_t>(loaded.format->bytesPerFrame());
    const auto startByte = last.offset() * loaded.format->sampleRate() / 1000 * stride;
    size_t decoded = 0;
    while(true)
    {
        const auto result = loaded.decoder->readAudio(4096);
        if(result.status == Fooyin::AudioDecoder::ReadStatus::EndOfStream)
        {
            break;
        }
        check(result.status == Fooyin::AudioDecoder::ReadStatus::DecodedAudio, "CUE waveform decoding failed");
        check(result.buffer.startTime() == (startByte + decoded) / stride * 1000 / loaded.format->sampleRate(),
              "CUE scan started at the wrong timestamp");
        compare(result.buffer, expected, startByte + decoded);
        decoded += result.buffer.byteCount();
    }
    check(decoded == static_cast<size_t>(expected.size()) - startByte, "CUE scan did not cover the final track");
    check(decoded >= loaded.format->bytesForDuration(last.duration()), "not enough audio for final waveform");
}

void verifyRegistry(Fooyin::InputPlugin *plugin, const QString &directory)
{
    Fooyin::AudioLoader loader;
    loader.addDecoder(plugin->inputName(), plugin->inputCreator().decoder);
    loader.addReader(plugin->inputName(), plugin->inputCreator().reader);
    const QString path = directory + QStringLiteral("/stereo16.tak");
    Fooyin::Track track{path};
    check(loader.supportedTrackExtensions().contains(QStringLiteral("tak")), "TAK not advertised by registry");
    check(loader.readTrackMetadata(track) && track.duration() > 0, "registry failed to read TAK metadata");
    auto loaded = loader.loadDecoderForTrack(track);
    check(loaded.decoder != nullptr, "fooyin registry failed to open TAK");
    check(loaded.name == plugin->inputName(), "fooyin selected a different TAK decoder");
    compare(loaded.decoder->readBuffer(4096), load(directory + QStringLiteral("/stereo16.s32le")), 0);
}

struct TagReaderState
{
    int calls{0};
    int initCalls{0};
    QString title;
    bool fail{false};
    QIODevice *expectedDevice{nullptr};
    Fooyin::ReaderCreator nested;
    std::weak_ptr<Fooyin::AudioLoader> nestedLoader;
};

class TagReader final : public Fooyin::AudioReader
{
  public:
    explicit TagReader(std::shared_ptr<TagReaderState> state) : m_state{std::move(state)} {}
    QStringList extensions() const override { return {QStringLiteral("tak")}; }
    bool canReadCover() const override { return true; }
    bool canWriteMetaData() const override { return false; }
    bool init(const Fooyin::AudioSource &source) override
    {
        ++m_state->initCalls;
        check(source.device && source.device->isOpen(), "tag reader did not receive an opened source");
        check(!m_state->expectedDevice || source.device == m_state->expectedDevice,
              "tag handoff replaced borrowed device");
        check(source.device->pos() == 0 && source.device->read(4) == QByteArray{"tBaK"},
              "tag handoff did not rewind source");
        return true;
    }
    bool readTrack(const Fooyin::AudioSource &source, Fooyin::Track &track) override
    {
        ++m_state->calls;
        if(m_state->nested)
        {
            const auto reader = m_state->nested();
            check(reader->init(source), "nested metadata init failed");
            return reader->readTrack(source, track);
        }
        if(const auto loader = m_state->nestedLoader.lock())
        {
            check(m_state->calls < 10, "registry metadata delegation recursed without a bound");
            return loader->readTrackMetadata(track);
        }
        check(track.title() != QStringLiteral("Failed tag title"), "failed reader changed track metadata");
        track.setTitle(m_state->title);
        track.setDuration(1);
        track.setSampleFrames(1);
        track.setSampleRate(1);
        track.setBitDepth(1);
        track.setRGTrackGain(-7.5F);
        track.setRGAlbumPeak(0.9F);
        track.replaceExtraTag(QStringLiteral("CUESHEET"), QStringLiteral("REM Synthetic embedded cue"));
        track.replaceExtraTag(QStringLiteral("CUSTOM"), QStringList{QStringLiteral("one"), QStringLiteral("two")});
        track.setTagTypes({QStringLiteral("APEv2")});
        return !m_state->fail;
    }
    QByteArray readCover(const Fooyin::AudioSource &, const Fooyin::Track &, Fooyin::Track::Cover) override
    {
        return QByteArray{"test artwork"};
    }

  private:
    std::shared_ptr<TagReaderState> m_state;
};

Fooyin::InputCreator configurePlugin(Fooyin::InputPlugin *plugin, Fooyin::CorePlugin *core,
                                     const std::shared_ptr<Fooyin::AudioLoader> &registry)
{
    const Fooyin::CorePluginContext context{nullptr, nullptr,  nullptr, nullptr, nullptr,
                                            nullptr, registry, nullptr, nullptr, nullptr};
    core->initialise(context);
    return plugin->inputCreator();
}

void verifyTagHandoff(Fooyin::InputPlugin *plugin, Fooyin::CorePlugin *core, const QString &directory)
{
    auto registry = std::make_shared<Fooyin::AudioLoader>();
    const auto creator = configurePlugin(plugin, core, registry);
    const auto failed = std::make_shared<TagReaderState>();
    failed->title = QStringLiteral("Failed tag title");
    failed->fail = true;
    const auto first = std::make_shared<TagReaderState>();
    first->title = QStringLiteral("First tag title");
    const auto second = std::make_shared<TagReaderState>();
    second->title = QStringLiteral("Second tag title");
    const auto recursive = std::make_shared<TagReaderState>();
    recursive->nested = creator.reader;
    const auto recursiveRegistry = std::make_shared<TagReaderState>();
    recursiveRegistry->nestedLoader = registry;
    registry->addReader(QStringLiteral("TAK"), creator.reader);
    registry->addReader(QStringLiteral("Recursive wrapper"),
                        [recursive] { return std::make_unique<TagReader>(recursive); });
    registry->addReader(QStringLiteral("Recursive registry wrapper"),
                        [recursiveRegistry] { return std::make_unique<TagReader>(recursiveRegistry); });
    registry->addReader(QStringLiteral("Failed reader"), [failed] { return std::make_unique<TagReader>(failed); });
    registry->addReader(QStringLiteral("First reader"), [first] { return std::make_unique<TagReader>(first); });
    registry->addReader(QStringLiteral("Second reader"), [second] { return std::make_unique<TagReader>(second); });
    const QString path = directory + QStringLiteral("/stereo16.tak");
    const QByteArray pcm = load(directory + QStringLiteral("/stereo16.s32le"));
    Fooyin::Track track{path};
    check(registry->readTrackMetadata(track), "tag delegation failed");
    check(track.title() == first->title && first->calls == 1 && second->calls == 0,
          "tag delegation ignored reader priority");
    check(recursive->calls == 1 && recursiveRegistry->calls == 1 && failed->calls == 1,
          "recursive/failing readers were not tried once");
    check(track.duration() == 2094 && track.sampleFrames() == static_cast<uint64_t>(pcm.size()) / 8 &&
              track.sampleRate() == 8000 && track.bitDepth() == 16,
          "delegate hid exact native audio properties");
    check(track.rgTrackGain() == -7.5F && track.rgAlbumPeak() == 0.9F &&
              track.extraTag(QStringLiteral("CUSTOM")) == QStringList{QStringLiteral("one"), QStringLiteral("two")} &&
              track.hasExtraTag(QStringLiteral("CUESHEET")) && track.tagTypes().contains(QStringLiteral("APEv2")),
          "native metadata hid tags or ReplayGain");
    check(track.tool() == QStringLiteral("TAK 2.3.3") && track.codecProfile() == QStringLiteral("-p4m"),
          "TAK encoder metadata missing");

    // Cover reading follows the host's independent canReadCover fall-through.
    check(registry->readTrackCover(track, Fooyin::Track::Cover::Front) == QByteArray{"test artwork"},
          "native reader hid artwork");
    registry->setReaderEnabled(QStringLiteral("First reader"), false);
    check(registry->readTrackMetadata(track) && track.title() == second->title,
          "disabled tag reader was still selected");
    registry->setReaderEnabled(QStringLiteral("First reader"), true);
    registry->changeReaderIndex(QStringLiteral("Second reader"), 1);
    check(registry->readTrackMetadata(track) && track.title() == second->title, "tag reader reordering ignored");

    // A supplied device may represent an archive member unavailable by path.
    QBuffer input;
    input.setData(load(path));
    check(input.open(QIODevice::ReadOnly) && input.seek(13), "could not prepare metadata device");
    second->expectedDevice = &input;
    Fooyin::AudioSource source;
    source.filepath = QStringLiteral("missing-archive-member.tak");
    source.device = &input;
    auto reader = creator.reader();
    check(reader->init(source) && input.pos() == 13, "metadata probe changed device position");
    check(reader->readTrack(source, track) && input.pos() == 13 && input.isOpen(),
          "tag delegation did not preserve caller device");
    const auto archiveWrapper = std::make_shared<TagReaderState>();
    archiveWrapper->title = QStringLiteral("Wrong archive wrapper");
    registry->addReader(
        QStringLiteral("Archive wrapper"), [archiveWrapper] { return std::make_unique<TagReader>(archiveWrapper); }, -1,
        true);
    const QString archivePath = QStringLiteral("/missing-test-archive.zip");
    Fooyin::Track archived{
        QStringLiteral("unpack://zip|%1|file://%2!music/track.tak").arg(archivePath.size()).arg(archivePath)};
    check(archived.isInArchive(), "test archive path invalid");
    check(reader->readTrack(source, archived) && archived.title() == second->title && archiveWrapper->initCalls == 0 &&
              input.pos() == 13 && input.isOpen(),
          "tag handoff reopened an archive instead of using its borrowed member");
    second->expectedDevice = nullptr;

    Fooyin::Track segment{track};
    segment.setCuePath(QStringLiteral("Embedded"));
    segment.setTitle(QStringLiteral("CUE title"));
    segment.setArtists({QStringLiteral("CUE artist")});
    segment.setSubsong(2);
    segment.setOffset(1000);
    segment.setDuration(1094);
    segment.setSampleFrames(8752);
    const int callsBefore = second->calls;
    check(reader->readTrack(source, segment), "CUE metadata read failed");
    check(segment.title() == QStringLiteral("CUE title") && segment.artist() == QStringLiteral("CUE artist") &&
              segment.offset() == 1000 && segment.duration() == 1094 && segment.sampleFrames() == 8752 &&
              segment.subsong() == 2 && segment.hasEmbeddedCue() && second->calls == callsBefore,
          "tag delegation changed a bounded CUE segment");
    segment.setCuePath({});
    segment.setIsChapter(true);
    check(reader->readTrack(source, segment) && segment.offset() == 1000 && segment.duration() == 1094 &&
              segment.sampleFrames() == 8752 && segment.title() == QStringLiteral("CUE title"),
          "native metadata changed a bounded chapter");

    registry->setReaderEnabled(QStringLiteral("First reader"), false);
    registry->setReaderEnabled(QStringLiteral("Second reader"), false);
    check(reader->readTrack(source, track) && track.title() == second->title && track.duration() == 2094,
          "failed tag readers prevented native metadata or changed existing tags");
    const std::weak_ptr<Fooyin::AudioLoader> weakRegistry = registry;
    registry.reset();
    check(weakRegistry.expired(), "plugin retained AudioLoader in an ownership cycle");
    check(reader->readTrack(source, track) && track.duration() == 2094, "expired loader broke native metadata");
}

void verifySequentialInput(const Fooyin::InputCreator &creator, const QString &directory)
{
    const QString path = directory + QStringLiteral("/stereo16.tak");
    const QByteArray data = load(path);
    const QByteArray expected = load(directory + QStringLiteral("/stereo16.s32le"));
    SequentialInput input{data};
    Fooyin::AudioSource source;
    source.filepath = QStringLiteral("synthetic-pipe.tak");
    source.device = &input;
    const auto reader = creator.reader();
    Fooyin::AudioSource fileSource;
    fileSource.filepath = path;
    check(reader->init(fileSource), "could not prime metadata reader state");
    check(!reader->init(source), "metadata probe accepted an irreversible sequential source");
    Fooyin::Track metadata{source.filepath};
    check(!reader->readTrack(source, metadata), "rejected pipe probe retained previous metadata");
    // Decoding this same pipe from its signature proves the rejected probe
    // neither consumed input nor closed the caller's device.
    const auto decoder = creator.decoder();
    check(decoder->init(source, Fooyin::Track{source.filepath}, Fooyin::AudioDecoder::VerifyIntegrity).has_value(),
          "sequential input was rejected");
    check(!decoder->isSeekable(), "pipe advertised seeking");
    size_t offset = 0;
    while(true)
    {
        const auto result = decoder->readAudio(997);
        if(result.status == Fooyin::AudioDecoder::ReadStatus::EndOfStream)
        {
            break;
        }
        check(result.status == Fooyin::AudioDecoder::ReadStatus::DecodedAudio, "sequential decoding failed");
        compare(result.buffer, expected, offset);
        offset += result.buffer.byteCount();
    }
    check(offset == static_cast<size_t>(expected.size()) && decoder->takeWarnings().isEmpty(),
          "sequential input lost samples or did not verify");
    decoder->stop();
    check(input.isOpen(), "decoder closed borrowed pipe");

    SequentialInput cancelled{data};
    source.device = &cancelled;
    const auto abortDecoder = creator.decoder();
    check(abortDecoder->init(source, {}, Fooyin::AudioDecoder::None).has_value(), "cancel source init failed");
    cancelled.beforeRead = [&abortDecoder] { abortDecoder->requestAbort(); };
    const auto result = abortDecoder->readAudio(4096);
    check(result.status == Fooyin::AudioDecoder::ReadStatus::Error &&
              result.error.contains(QStringLiteral("cancelled")),
          "cancellation during source read was ignored");

    FailingSeekBuffer broken;
    broken.setData(data);
    check(broken.open(QIODevice::ReadOnly), "could not open seek failure source");
    source.device = &broken;
    const auto seekDecoder = creator.decoder();
    check(seekDecoder->init(source, {}, Fooyin::AudioDecoder::None).has_value(), "seek failure source init failed");
    broken.failSeek = true;
    seekDecoder->seek(1000);
    check(seekDecoder->readAudio(4096).status == Fooyin::AudioDecoder::ReadStatus::Error,
          "seek failure was hidden from playback");
    broken.failSeek = false;
    seekDecoder->seek(0);
    compare(seekDecoder->readBuffer(4096), expected, 0);
    seekDecoder->requestAbort();
    seekDecoder->seek(1000);
    check(seekDecoder->readAudio(4096).status == Fooyin::AudioDecoder::ReadStatus::Error,
          "cancelled seek continued decoding");
}

QByteArray changeMetadata(QByteArray data, unsigned type, const std::function<void(char *, size_t)> &change)
{
    size_t position = 4;
    while(position + 4 <= static_cast<size_t>(data.size()))
    {
        const auto *header = reinterpret_cast<const uint8_t *>(data.constData() + position);
        const size_t size = header[1] | (size_t{header[2]} << 8) | (size_t{header[3]} << 16);
        check(position + 4 + size <= static_cast<size_t>(data.size()), "bad test metadata size");
        if((header[0] & 0x7f) == type)
        {
            check(size >= 3, "missing metadata checksum");
            char *block = data.data() + position + 4;
            change(block, size - 3);
            uint32_t crc = 0xb704ce;
            for(size_t index = 0; index < size - 3; ++index)
            {
                crc ^= uint32_t{static_cast<uint8_t>(block[index])} << 16;
                for(int bit = 0; bit < 8; ++bit)
                {
                    crc = (crc << 1) ^ ((crc & 0x800000) ? 0x864cfbU : 0U);
                }
            }
            block[size - 3] = static_cast<char>(crc);
            block[size - 2] = static_cast<char>(crc >> 8);
            block[size - 1] = static_cast<char>(crc >> 16);
            return data;
        }
        check((header[0] & 0x7f) != 0, "test metadata not found");
        position += 4 + size;
    }
    throw std::runtime_error("test metadata not found");
}

Fooyin::AudioDecoder::ReadResult drain(Fooyin::AudioDecoder &decoder)
{
    while(true)
    {
        auto result = decoder.readAudio(4096);
        if(result.status != Fooyin::AudioDecoder::ReadStatus::DecodedAudio)
        {
            return result;
        }
    }
}

void verifyIntegrityAndUnknownLength(const Fooyin::InputCreator &creator, const QString &directory)
{
    const QByteArray original = load(directory + QStringLiteral("/stereo16.tak"));
    QByteArray wrongMd5 = changeMetadata(original, 6, [](char *block, size_t) { block[0] ^= 1; });
    QBuffer input{&wrongMd5};
    check(input.open(QIODevice::ReadOnly), "could not open MD5 test source");
    Fooyin::AudioSource source;
    source.filepath = QStringLiteral("integrity-test.tak");
    source.device = &input;
    const Fooyin::Track track{source.filepath};
    const auto decoder = creator.decoder();
    check(decoder->init(source, track, Fooyin::AudioDecoder::None).has_value(), "MD5 metadata rejected at open");
    check(drain(*decoder).status == Fooyin::AudioDecoder::ReadStatus::EndOfStream,
          "optional MD5 verification ignored host flag");
    decoder->stop();
    check(decoder->init(source, track, Fooyin::AudioDecoder::VerifyIntegrity).has_value(),
          "integrity decoder failed to initialise");
    const auto result = drain(*decoder);
    check(result.status == Fooyin::AudioDecoder::ReadStatus::Error &&
              result.error.contains(QStringLiteral("MD5"), Qt::CaseInsensitive),
          "VerifyIntegrity failed to detect stored PCM MD5 mismatch");

    QBuffer good;
    good.setData(original);
    check(good.open(QIODevice::ReadOnly), "could not open integrity fixture");
    source.device = &good;
    const auto traversal = creator.decoder();
    check(traversal->init(source, track, Fooyin::AudioDecoder::VerifyIntegrity).has_value(),
          "valid integrity source failed");
    traversal->seek(13);
    check(drain(*traversal).status == Fooyin::AudioDecoder::ReadStatus::EndOfStream,
          "partial MD5 traversal failed instead of becoming incomplete");
    check(!traversal->takeWarnings().isEmpty() && traversal->takeWarnings().isEmpty(),
          "partial verification warning missing or not drained");
    traversal->seek(0);
    check(drain(*traversal).status == Fooyin::AudioDecoder::ReadStatus::EndOfStream &&
              traversal->takeWarnings().isEmpty(),
          "full MD5 verification did not restart after seeking to zero");

    QBuffer legacy;
    legacy.setData(load(directory + QStringLiteral("/legacy-stereo16.tak")));
    check(legacy.open(QIODevice::ReadOnly), "could not open legacy integrity source");
    source.device = &legacy;
    const auto withoutMd5 = creator.decoder();
    check(withoutMd5->init(source, track, Fooyin::AudioDecoder::VerifyIntegrity).has_value() &&
              drain(*withoutMd5).status == Fooyin::AudioDecoder::ReadStatus::EndOfStream,
          "legacy CRC verification failed");
    const auto warnings = withoutMd5->takeWarnings();
    check(warnings.size() == 1 && warnings.front().contains(QStringLiteral("no stored PCM MD5")),
          "missing MD5 was reported as whole-PCM verification");

    QByteArray unknownLength = changeMetadata(original, 1,
                                              [](char *block, size_t size)
                                              {
                                                  check(size >= 7, "test stream info too short");
                                                  // The stored sample count is the 35-bit field following codec,
                                                  // profile and frame size (6 + 4 + 4 bits).
                                                  for(size_t bit = 14; bit < 49; ++bit)
                                                  {
                                                      block[bit / 8] = static_cast<char>(
                                                          static_cast<uint8_t>(block[bit / 8]) & ~(1U << (bit % 8)));
                                                  }
                                              });
    SequentialInput pipe{unknownLength};
    source.device = &pipe;
    const auto streaming = creator.decoder();
    const auto options = Fooyin::AudioDecoder::UpdateTracks | Fooyin::AudioDecoder::VerifyIntegrity;
    check(streaming->init(source, track, options).has_value(), "unknown-length stream rejected");
    check(streaming->trackHasChanged() && streaming->changedTrack().sampleFrames() == 0,
          "unknown-length stream invented a sample count");
    const auto pcm = load(directory + QStringLiteral("/stereo16.s32le"));
    size_t offset = 0;
    while(true)
    {
        const auto read = streaming->readAudio(4096);
        if(read.status == Fooyin::AudioDecoder::ReadStatus::EndOfStream)
        {
            break;
        }
        check(read.status == Fooyin::AudioDecoder::ReadStatus::DecodedAudio, "unknown-length decoding failed");
        compare(read.buffer, pcm, offset);
        offset += read.buffer.byteCount();
    }
    check(offset == static_cast<size_t>(pcm.size()) && streaming->takeWarnings().isEmpty(),
          "unknown-length stream failed PCM/MD5 validation");
    check(streaming->trackHasChanged(), "resolved duration was not exposed to host");
    const Fooyin::Track resolved = streaming->changedTrack();
    check(resolved.sampleFrames() == static_cast<uint64_t>(pcm.size()) / 8 && resolved.duration() == 2094 &&
              resolved.tool() == QStringLiteral("TAK 2.3.3") && !streaming->trackHasChanged(),
          "resolved stream properties incorrect");
}
} // namespace

int main(int argc, char **argv)
{
    QCoreApplication app{argc, argv};
    if(argc != 3)
    {
        std::cerr << "usage: tak_plugin_test PLUGIN FIXTURE_DIRECTORY\n";
        return 2;
    }
    try
    {
        QPluginLoader loader{QString::fromLocal8Bit(argv[1])};
        auto *object = loader.instance();
        if(!object)
        {
            throw std::runtime_error(loader.errorString().toStdString());
        }
        check(qobject_cast<Fooyin::Plugin *>(object) != nullptr, "not a fooyin plugin");
        auto *plugin = qobject_cast<Fooyin::InputPlugin *>(object);
        check(plugin != nullptr, "missing fooyin input interface");
        check(plugin->inputName() == QStringLiteral("TAK"), "wrong plugin identity");
        auto *core = qobject_cast<Fooyin::CorePlugin *>(object);
        check(core != nullptr, "missing core metadata registry interface");
        const auto creator = plugin->inputCreator();
        check(static_cast<bool>(creator.decoder), "missing decoder factory");
        check(static_cast<bool>(creator.reader), "missing metadata reader factory");
        const QString directory = QString::fromLocal8Bit(argv[2]);
        for(const auto *name :
            {"mono8", "stereo8", "stereo16", "surround24", "silence16", "legacy-mono8", "legacy-stereo8",
             "legacy-stereo16", "legacy-stereo24", "legacy-silence16", "legacy-negative24"})
        {
            for(bool deviceSource : {false, true})
            {
                verifyDecoder(creator, directory, QString::fromLatin1(name), deviceSource);
            }
        }
        verifyErrors(creator, directory);
        verifyReader(creator, directory, false);
        verifyReader(creator, directory, true);
        verifyCueScan(creator, directory);
        verifyRegistry(plugin, directory);
        verifyTagHandoff(plugin, core, directory);
        verifySequentialInput(creator, directory);
        verifyIntegrityAndUnknownLength(creator, directory);
        std::cout << "Plugin loaded; metadata, CUE scans, PCM, timestamps, seeking, EOF, cancellation, reinit, device "
                     "ownership, and error "
                     "handling passed\n";
        return 0;
    }
    catch(const std::exception &error)
    {
        std::cerr << error.what() << '\n';
        return 1;
    }
}
