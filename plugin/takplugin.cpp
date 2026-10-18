// SPDX-License-Identifier: GPL-3.0-or-later
#include "takplugin.h"
#include "fooyin_tak.h"

#include <core/engine/audioloader.h>
#include <core/plugins/coreplugincontext.h>

#include <QDebug>
#include <QFile>
#include <QFileInfo>
#include <QIODevice>
#include <QScopedValueRollback>

#include <algorithm>
#include <limits>
#include <memory>
#include <vector>

namespace
{
// Readers receive borrowed devices. Probing and tag delegation must leave their
// cursor where it was so the caller can reuse the source or try another reader.
class DevicePosition final
{
  public:
    explicit DevicePosition(QIODevice *device)
        : m_device{device && !device->isSequential() ? device : nullptr}, m_position{m_device ? m_device->pos() : -1}
    {
    }
    ~DevicePosition()
    {
        if(m_device && m_position >= 0)
        {
            m_device->seek(m_position);
        }
    }

  private:
    QIODevice *m_device;
    qint64 m_position;
};

void applyAudioProperties(Fooyin::Track &track, const TakStreamInfo &info, int32_t validBits)
{
    // A CUE/chapter's duration and sample count describe its segment, not the
    // complete TAK image. Its title, tags and offset are also authoritative.
    if(!track.isBoundedSegment())
    {
        track.setDuration(uint64_t{info.total_samples} * 1000 / info.sample_rate);
        track.setSampleFrames(info.total_samples);
    }
    track.setSampleRate(static_cast<int>(info.sample_rate));
    track.setChannels(static_cast<int>(info.channels));
    track.setBitDepth(static_cast<int>(info.bits_per_sample));
    if(validBits > 0 && validBits != info.bits_per_sample)
    {
        track.setExtraProperty(QStringLiteral("TAK valid bits"), QString::number(validBits));
    }
    else
    {
        track.removeExtraProperty(QStringLiteral("TAK valid bits"));
    }
    track.setCodec(QStringLiteral("TAK"));
    track.setEncoding(QStringLiteral("Lossless"));
}

void applyEncoderProperties(Fooyin::Track &track, const std::optional<TakEncoderInfo> &encoder, int32_t codec,
                            bool hasMd5)
{
    track.setExtraProperty(QStringLiteral("TAK codec"), QString::number(codec));
    track.setExtraProperty(QStringLiteral("TAK PCM MD5"),
                           hasMd5 ? QStringLiteral("Stored") : QStringLiteral("Not stored"));
    if(encoder)
    {
        const auto version = encoder->version;
        track.setTool(QStringLiteral("TAK %1.%2.%3").arg(version >> 16).arg((version >> 8) & 255).arg(version & 255));
        const QString suffix = encoder->evaluation == 1   ? QStringLiteral("e")
                               : encoder->evaluation == 2 ? QStringLiteral("m")
                                                          : QString{};
        track.setCodecProfile(QStringLiteral("-p%1%2").arg(encoder->preset).arg(suffix));
        if(encoder->evaluation < 0)
        {
            track.setExtraProperty(QStringLiteral("TAK evaluation"), QStringLiteral("Unknown"));
        }
        else
        {
            track.removeExtraProperty(QStringLiteral("TAK evaluation"));
        }
    }
}

QString verificationText(int32_t status)
{
    switch(status)
    {
    case TAK_VERIFICATION_NOT_PRESENT:
        return QStringLiteral("Not stored");
    case TAK_VERIFICATION_DISABLED:
        return QStringLiteral("Disabled");
    case TAK_VERIFICATION_PENDING:
        return QStringLiteral("Pending full decode");
    case TAK_VERIFICATION_VERIFIED:
        return QStringLiteral("Verified");
    case TAK_VERIFICATION_MISMATCH:
        return QStringLiteral("Mismatch");
    case TAK_VERIFICATION_INCOMPLETE:
        return QStringLiteral("Incomplete traversal");
    default:
        return {};
    }
}

class TakDecoder final : public Fooyin::AudioDecoder
{
  public:
    ~TakDecoder() override { stop(); }

    QStringList extensions() const override { return {QStringLiteral("tak")}; }
    QStringList preferredExtensions() const override { return extensions(); }
    bool isSeekable() const override { return m_handle && !m_noSeeking && tak_is_seekable(m_handle); }

    const TakStreamInfo &streamInfo() const { return m_info; }
    std::optional<TakEncoderInfo> encoderInfo() const
    {
        TakEncoderInfo info{};
        return tak_encoder_info(m_handle, &info) == 1 ? std::optional{info} : std::nullopt;
    }
    int32_t codec() const { return tak_codec(m_handle); }
    int32_t validBits() const { return tak_valid_bits(m_handle); }
    bool hasMd5() const { return tak_verification(m_handle) != TAK_VERIFICATION_NOT_PRESENT; }
    bool trackHasChanged() const override { return m_trackChanged; }
    Fooyin::Track changedTrack() const override
    {
        m_trackChanged = false;
        return m_track;
    }
    QStringList takeWarnings() override
    {
        if(!m_handle || !m_verifyMd5)
        {
            return {};
        }
        const int32_t status = tak_verification(m_handle);
        if(status == m_warnedStatus)
        {
            return {};
        }
        m_warnedStatus = status;
        if(status == TAK_VERIFICATION_NOT_PRESENT)
        {
            return {QStringLiteral("TAK has no stored PCM MD5; only frame CRC validation is available.")};
        }
        if(status == TAK_VERIFICATION_PENDING || status == TAK_VERIFICATION_INCOMPLETE)
        {
            return {QStringLiteral("TAK whole-file PCM MD5 was not verified: verification requires decoding "
                                   "the complete file without seeking.")};
        }
        return {};
    }

    std::optional<Fooyin::AudioFormat> init(const Fooyin::AudioSource &source, const Fooyin::Track &track,
                                            DecoderOptions options) override
    {
        stop();
        m_noSeeking = options.testFlag(NoSeeking);
        m_verifyMd5 = options.testFlag(VerifyIntegrity);
        m_updateTrack = options.testFlag(UpdateTracks);
        m_track = track;
        m_device = source.device;
        if(!m_device)
        {
            m_file = std::make_unique<QFile>(source.filepath);
            if(!m_file->open(QIODevice::ReadOnly))
            {
                qWarning() << "TAK: could not open" << source.filepath << m_file->errorString();
                stop();
                return std::nullopt;
            }
            m_device = m_file.get();
        }
        if(!m_device->isReadable())
        {
            qWarning() << "TAK requires a readable input device";
            stop();
            return std::nullopt;
        }
        char error[512]{};
        const TakIo io{this, readInput, m_device->isSequential() ? nullptr : seekInput};
        const TakOptions decoderOptions{m_verifyMd5 ? 1U : 0U, TAK_RECOVERY_STRICT};
        m_handle = tak_open_with_options(io, &decoderOptions, &m_info, error, sizeof(error));
        if(!m_handle)
        {
            qWarning() << "TAK:" << error;
            stop();
            return std::nullopt;
        }
        m_format = Fooyin::AudioFormat{Fooyin::SampleFormat::S32, static_cast<int>(m_info.sample_rate),
                                       static_cast<int>(m_info.channels)};
        // TAK speaker identifiers 1..18 match fooyin's channel positions.
        Fooyin::AudioFormat::ChannelLayout layout;
        for(uint16_t i = 0; i < m_info.channels; ++i)
        {
            layout.push_back(static_cast<Fooyin::AudioFormat::ChannelPosition>(m_info.channel_layout[i]));
        }
        if(std::all_of(layout.begin(), layout.end(),
                       [](auto position) { return position != Fooyin::AudioFormat::ChannelPosition::UnknownPosition; }))
        {
            if(!m_format.setChannelLayout(layout))
            {
                m_format.clearChannelLayout();
            }
        }
        else if(m_info.channels > 2)
        {
            m_format.clearChannelLayout();
        }
        // WaveBar and ReplayGain scans request NoSeeking, but still need the
        // initial CUE segment offset. Position once before disabling seeks.
        if(m_noSeeking && track.offset() > 0 && tak_seek(m_handle, sampleForTime(track.offset())) != 0)
        {
            qWarning() << "TAK:" << tak_last_error(m_handle);
            stop();
            return std::nullopt;
        }
        updateTrack();
        return m_format;
    }

    void stop() override
    {
        tak_close(m_handle);
        m_handle = nullptr;
        m_device = nullptr;
        m_file.reset();
        m_samples.clear();
        m_info = {};
        m_format = {};
        m_noSeeking = false;
        m_error.clear();
        m_verifyMd5 = false;
        m_updateTrack = false;
        m_trackChanged = false;
        m_warnedStatus = -1;
        m_reportedVerification = -1;
        m_track = {};
    }

    void seek(uint64_t milliseconds) override
    {
        if(!isSeekable())
        {
            return;
        }
        if(abortToken().stop_requested())
        {
            m_error = QStringLiteral("TAK decoding cancelled");
        }
        else if(tak_seek(m_handle, sampleForTime(milliseconds)) != 0)
        {
            m_error = QString::fromUtf8(tak_last_error(m_handle));
        }
        else
        {
            m_error.clear();
        }
        updateTrack();
    }

    ReadResult readAudio(size_t bytes) override
    {
        if(!m_handle)
        {
            return ReadResult::errorResult(QStringLiteral("TAK decoder is not initialised"));
        }
        if(abortToken().stop_requested())
        {
            return ReadResult::errorResult(QStringLiteral("TAK decoding cancelled"));
        }
        if(!m_error.isEmpty())
        {
            return ReadResult::errorResult(m_error);
        }
        const size_t stride = size_t{m_info.channels} * sizeof(int32_t);
        if(bytes < stride)
        {
            return ReadResult::errorResult(QStringLiteral("TAK output request is smaller than one sample frame"));
        }
        const size_t frames = std::min(bytes / stride, size_t{16384});
        m_samples.resize(frames * m_info.channels);
        const uint64_t timestamp = tak_position(m_handle) * 1000 / m_info.sample_rate;
        const int64_t count = tak_read(m_handle, m_samples.data(), m_samples.size());
        if(count < 0)
        {
            m_error = abortToken().stop_requested() ? QStringLiteral("TAK decoding cancelled")
                                                    : QString::fromUtf8(tak_last_error(m_handle));
            updateTrack();
            return ReadResult::errorResult(m_error);
        }
        tak_info(m_handle, &m_info);
        updateTrack();
        if(count == 0)
        {
            return ReadResult::endOfStream();
        }
        return ReadResult::data(Fooyin::AudioBuffer{reinterpret_cast<const uint8_t *>(m_samples.data()),
                                                    static_cast<size_t>(count) * sizeof(int32_t), m_format, timestamp});
    }

    Fooyin::AudioBuffer readBuffer(size_t bytes) override { return readAudio(bytes).buffer; }

  private:
    void updateTrack()
    {
        if(!m_handle || !m_updateTrack)
        {
            return;
        }
        const int32_t verification = tak_verification(m_handle);
        if(m_reportedVerification == verification && m_reportedSamples == m_info.total_samples)
        {
            return;
        }
        m_reportedVerification = verification;
        m_reportedSamples = m_info.total_samples;
        const Fooyin::Track before{m_track};
        applyAudioProperties(m_track, m_info, validBits());
        applyEncoderProperties(m_track, encoderInfo(), codec(), hasMd5());
        m_track.setExtraProperty(QStringLiteral("TAK verification"), verificationText(verification));
        m_trackChanged = m_trackChanged || !before.sameDataAs(m_track);
    }

    uint64_t sampleForTime(uint64_t milliseconds) const
    {
        // Clamp before multiplying, so even UINT64_MAX is a safe EOF seek.
        if(tak_sample_count_known(m_handle))
        {
            const uint64_t endMs =
                (uint64_t{m_info.total_samples} * 1000 + m_info.sample_rate - 1) / m_info.sample_rate;
            if(milliseconds >= endMs)
            {
                return m_info.total_samples;
            }
        }
        const uint64_t seconds = milliseconds / 1000;
        if(seconds > std::numeric_limits<uint64_t>::max() / m_info.sample_rate)
        {
            return std::numeric_limits<uint64_t>::max();
        }
        const uint64_t samples = seconds * m_info.sample_rate;
        const uint64_t remainder = milliseconds % 1000 * m_info.sample_rate / 1000;
        return samples > std::numeric_limits<uint64_t>::max() - remainder ? std::numeric_limits<uint64_t>::max()
                                                                          : samples + remainder;
    }

    static int64_t readInput(void *context, uint8_t *output, size_t size) noexcept
    {
        try
        {
            auto *self = static_cast<TakDecoder *>(context);
            if(self->abortToken().stop_requested() || size > static_cast<size_t>(std::numeric_limits<qint64>::max()))
            {
                return -1;
            }
            return self->m_device->read(reinterpret_cast<char *>(output), static_cast<qint64>(size));
        }
        catch(...)
        {
            return -1;
        }
    }

    static int64_t seekInput(void *context, int64_t offset, int32_t origin) noexcept
    {
        try
        {
            auto *self = static_cast<TakDecoder *>(context);
            if(self->abortToken().stop_requested())
            {
                return -1;
            }
            auto *device = self->m_device;
            qint64 base = 0;
            if(origin == 1)
            {
                base = device->pos();
            }
            else if(origin == 2)
            {
                base = device->size();
            }
            else if(origin != 0)
            {
                return -1;
            }
            if(base < 0 || offset < -base || (offset > 0 && base > std::numeric_limits<qint64>::max() - offset))
            {
                return -1;
            }
            const qint64 position = base + offset;
            return device->seek(position) ? position : -1;
        }
        catch(...)
        {
            return -1;
        }
    }

    TakHandle *m_handle{nullptr};
    TakStreamInfo m_info{};
    QIODevice *m_device{nullptr};
    std::unique_ptr<QFile> m_file;
    Fooyin::AudioFormat m_format;
    std::vector<int32_t> m_samples;
    bool m_noSeeking{false};
    QString m_error;
    bool m_verifyMd5{false};
    bool m_updateTrack{false};
    mutable bool m_trackChanged{false};
    int32_t m_warnedStatus{-1};
    int32_t m_reportedVerification{-1};
    uint64_t m_reportedSamples{0};
    Fooyin::Track m_track;
};

// Prefer native TAK properties and delegate tags to configured readers. CUE
// tracks inherit the exact image duration, including the final track which has
// no following INDEX from which to calculate its end.
class TakReader final : public Fooyin::AudioReader
{
  public:
    explicit TakReader(std::weak_ptr<Fooyin::AudioLoader> loader) : m_audioLoader{std::move(loader)} {}

    QStringList extensions() const override { return {QStringLiteral("tak")}; }
    QStringList preferredExtensions() const override { return extensions(); }
    bool canReadCover() const override { return false; }
    bool canWriteMetaData() const override { return false; }

    bool init(const Fooyin::AudioSource &source) override
    {
        m_info = {};
        m_fileSize = 0;
        m_encoder.reset();
        // A temporary decoder can read ahead while probing. A pipe cannot be
        // rewound to return those bytes to its owner or the next reader.
        // Sequential playback is supported by the persistent decoder instead.
        if(source.device && source.device->isSequential())
        {
            return false;
        }
        const DevicePosition restorePosition{source.device};
        TakDecoder probe;
        if(!probe.init(source, {}, Fooyin::AudioDecoder::None))
        {
            return false;
        }
        m_info = probe.streamInfo();
        m_encoder = probe.encoderInfo();
        m_codec = probe.codec();
        m_validBits = probe.validBits();
        m_hasMd5 = probe.hasMd5();
        const qint64 size = source.device ? source.device->size() : QFileInfo{source.filepath}.size();
        if(size > 0)
        {
            m_fileSize = static_cast<uint64_t>(size);
        }
        return true;
    }

    bool readTrack(const Fooyin::AudioSource &source, Fooyin::Track &track) override
    {
        // A peer may itself call AudioLoader::readTrackMetadata. Let a nested
        // TAK read satisfy that request with technical properties, then discard
        // the peer result outside. Returning false here would let that loader
        // retry the same composing peer recursively without a bound.
        static thread_local bool delegating{false};
        static thread_local bool reentered{false};
        if(m_info.sample_rate == 0)
        {
            return false;
        }
        if(delegating)
        {
            reentered = true;
        }
        else if(!track.isBoundedSegment())
        {
            const QScopedValueRollback<bool> guard{delegating, true};
            readTags(source, track, reentered);
        }
        const uint64_t duration = uint64_t{m_info.total_samples} * 1000 / m_info.sample_rate;
        applyAudioProperties(track, m_info, m_validBits);
        applyEncoderProperties(track, m_encoder, m_codec, m_hasMd5);
        track.setFileSize(m_fileSize);
        track.setBitrate(duration > 0 ? static_cast<int>(std::min(double(m_fileSize) * 8 / double(duration),
                                                                  double(std::numeric_limits<int>::max())))
                                      : 0);
        return true;
    }

  private:
    void readTags(const Fooyin::AudioSource &source, Fooyin::Track &track, bool &reentered) const
    {
        const auto loader = m_audioLoader.lock();
        if(!loader || (source.device && source.device->isSequential()))
        {
            return;
        }
        Fooyin::AudioSource tagSource{source};
        QFile file{source.filepath};
        if(!tagSource.device)
        {
            if(!file.open(QIODevice::ReadOnly))
            {
                return;
            }
            tagSource.device = &file;
        }
        const DevicePosition restorePosition{tagSource.device};
        // This registry method honours disabled readers and configured order.
        // Keep the caller's device, including an already opened archive member.
        // An archive member is already open here. Selecting by its unpack://
        // URL would select the archive wrapper again, reopening the container.
        const auto readers =
            track.isInArchive() ? loader->readersForFile(track.pathInArchive()) : loader->readersForTrack(track);
        for(const auto &reader : readers)
        {
            if(dynamic_cast<TakReader *>(reader.get()))
            {
                continue;
            }
            if(!tagSource.device->seek(0))
            {
                break;
            }
            Fooyin::Track candidate{track};
            const QScopedValueRollback<bool> nestedGuard{reentered, false};
            if(reader->init(tagSource) && reader->readTrack(tagSource, candidate) && !reentered)
            {
                track = std::move(candidate);
                break;
            }
        }
    }

    std::weak_ptr<Fooyin::AudioLoader> m_audioLoader;
    TakStreamInfo m_info{};
    uint64_t m_fileSize{0};
    std::optional<TakEncoderInfo> m_encoder;
    int32_t m_codec{0};
    int32_t m_validBits{0};
    bool m_hasMd5{false};
};
} // namespace

QString TakPlugin::inputName() const { return QStringLiteral("TAK"); }

Fooyin::InputCreator TakPlugin::inputCreator() const
{
    Fooyin::InputCreator creator;
    creator.decoder = [] { return std::make_unique<TakDecoder>(); };
    creator.reader = [loader = m_audioLoader] { return std::make_unique<TakReader>(loader); };
    return creator;
}

void TakPlugin::initialise(const Fooyin::CorePluginContext &context) { m_audioLoader = context.audioLoader; }
