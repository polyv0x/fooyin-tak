// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

// fooyin 0.13.1's audioformat.h needs <vector> to be included first.
#include <vector>

#include <core/engine/inputplugin.h>
#include <core/plugins/coreplugin.h>
#include <core/plugins/plugin.h>

#include <QObject>
#include <memory>

namespace Fooyin
{
class AudioLoader;
}

class TakPlugin final : public QObject, public Fooyin::Plugin, public Fooyin::InputPlugin, public Fooyin::CorePlugin
{
    Q_OBJECT
    Q_PLUGIN_METADATA(IID "org.fooyin.fooyin.plugin/1.0" FILE "tak.json")
    Q_INTERFACES(Fooyin::Plugin Fooyin::InputPlugin Fooyin::CorePlugin)

  public:
    [[nodiscard]] QString inputName() const override;
    [[nodiscard]] Fooyin::InputCreator inputCreator() const override;
    void initialise(const Fooyin::CorePluginContext &context) override;

  private:
    std::weak_ptr<Fooyin::AudioLoader> m_audioLoader;
};
