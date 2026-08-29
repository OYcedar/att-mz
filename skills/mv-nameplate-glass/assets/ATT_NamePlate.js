/*:
 * @plugindesc v1.0 为 RPG Maker MV 显示宽度自适应的玻璃胶囊人物姓名牌
 * @author ATT
 *
 * @param FontFace
 * @text 姓名字体
 * @type string
 * @default GameFont
 *
 * @param FontSize
 * @text 姓名字号
 * @type number
 * @min 16
 * @default 28
 *
 * @param MinWidth
 * @text 最小宽度
 * @type number
 * @min 96
 * @default 168
 *
 * @param MaxWidth
 * @text 最大宽度
 * @type number
 * @min 160
 * @default 420
 *
 * @param PlateHeight
 * @text 姓名牌高度
 * @type number
 * @min 44
 * @default 58
 *
 * @param OffsetX
 * @text 相对对话框左偏移
 * @type number
 * @default 72
 *
 * @param Gap
 * @text 与对话框间距
 * @type number
 * @min 0
 * @default 4
 *
 * @help
 * 把本插件放在当前消息插件之后。
 *
 * 自定义消息系统在加入正文前调用：
 *   $gameMessage.setATTNamePlate('茵斯蒂');
 *   $gameMessage.add('台词正文');
 *
 * 旁白使用空姓名：
 *   $gameMessage.setATTNamePlate('');
 *
 * 本插件会自动连接 TS_ADVsystem 的 [姓名]正文 格式。
 */

var Imported = Imported || {};
Imported.ATT_NamePlate = true;

(function() {
    'use strict';

    var pluginName = 'ATT_NamePlate';
    var parameters = PluginManager.parameters(pluginName);

    function numberParameter(name, fallback, minimum) {
        var value = Number(parameters[name]);
        if (!isFinite(value)) {
            return fallback;
        }
        return Math.max(minimum, value);
    }

    function offsetParameter(name, fallback) {
        var value = Number(parameters[name]);
        return isFinite(value) ? value : fallback;
    }

    function clamp(value, minimum, maximum) {
        return Math.max(minimum, Math.min(maximum, value));
    }

    function drawRoundedPath(context, x, y, width, height, radius) {
        var r = Math.min(radius, width / 2, height / 2);
        context.beginPath();
        context.moveTo(x + r, y);
        context.lineTo(x + width - r, y);
        context.quadraticCurveTo(x + width, y, x + width, y + r);
        context.lineTo(x + width, y + height - r);
        context.quadraticCurveTo(x + width, y + height, x + width - r, y + height);
        context.lineTo(x + r, y + height);
        context.quadraticCurveTo(x, y + height, x, y + height - r);
        context.lineTo(x, y + r);
        context.quadraticCurveTo(x, y, x + r, y);
        context.closePath();
    }

    function speakerFromTsText(text) {
        if (typeof text !== 'string' || text.charAt(0) !== '[') {
            return '';
        }
        var end = text.indexOf(']');
        if (end <= 1) {
            return '';
        }
        var name = text.slice(1, end);
        var voiceSeparator = name.indexOf('/');
        if (voiceSeparator >= 0) {
            name = name.slice(0, voiceSeparator);
        }
        return name.trim();
    }

    function readTsMessage(text) {
        if (
            typeof text !== 'string' ||
            typeof $advSystem === 'undefined' ||
            !$advSystem ||
            typeof $advSystem.isRun !== 'function' ||
            !$advSystem.isRun()
        ) {
            return null;
        }

        var rawName = speakerFromTsText(text);
        if (rawName) {
            return {
                name: rawName,
                body: text.slice(text.indexOf(']') + 1)
            };
        }

        var prefix = $advSystem.F_SPACE;
        if (typeof prefix !== 'string' || !prefix || text.indexOf(prefix) !== 0) {
            return null;
        }

        var firstLineEnd = text.indexOf('\n');
        if (firstLineEnd <= prefix.length) {
            return null;
        }

        var name = text.slice(prefix.length, firstLineEnd).trim();
        var body = text.slice(firstLineEnd + 1);
        if (!name || body.indexOf(prefix) !== 0) {
            return null;
        }

        return {
            name: name,
            body: body
        };
    }

    var fontFace = String(parameters.FontFace || 'GameFont');
    var fontSize = numberParameter('FontSize', 28, 16);
    var minWidth = numberParameter('MinWidth', 168, 96);
    var maxWidth = numberParameter('MaxWidth', 420, 160);
    var plateHeight = numberParameter('PlateHeight', 58, 44);
    var offsetX = offsetParameter('OffsetX', 72);
    var gap = numberParameter('Gap', 4, 0);

    var _Game_Message_clear = Game_Message.prototype.clear;
    Game_Message.prototype.clear = function() {
        _Game_Message_clear.call(this);
        this._attNamePlateName = '';
        this._attNamePlateExplicit = false;
    };

    Game_Message.prototype.setATTNamePlate = function(name) {
        this._attNamePlateName = String(name || '').trim();
        this._attNamePlateExplicit = this._attNamePlateName.length > 0;
    };

    Game_Message.prototype.attNamePlate = function() {
        return this._attNamePlateName || '';
    };

    var _Game_Message_add = Game_Message.prototype.add;
    Game_Message.prototype.add = function(text) {
        var tsMessage = readTsMessage(text);
        var textCount = this._texts.length;

        if (!tsMessage) {
            if (textCount === 0 && !this._attNamePlateExplicit) {
                this._attNamePlateName = '';
            }
            _Game_Message_add.call(this, text);
            return;
        }

        _Game_Message_add.call(this, text);
        this.setATTNamePlate(tsMessage.name);

        if (this._texts.length > textCount) {
            this._texts[this._texts.length - 1] = '\n' + tsMessage.body;
        }
    };

    function Window_ATTNamePlate() {
        this.initialize.apply(this, arguments);
    }

    Window_ATTNamePlate.prototype = Object.create(Window_Base.prototype);
    Window_ATTNamePlate.prototype.constructor = Window_ATTNamePlate;

    Window_ATTNamePlate.prototype.initialize = function(messageWindow) {
        this._messageWindow = messageWindow;
        Window_Base.prototype.initialize.call(this, 0, 0, minWidth, plateHeight);
        this.opacity = 255;
        this.backOpacity = 255;
        this.openness = 0;
        this.deactivate();
    };

    Window_ATTNamePlate.prototype.standardPadding = function() {
        return 10;
    };

    Window_ATTNamePlate.prototype.standardFontFace = function() {
        return fontFace;
    };

    Window_ATTNamePlate.prototype.standardFontSize = function() {
        return fontSize;
    };

    Window_ATTNamePlate.prototype._refreshBack = function() {
        var width = this._width;
        var height = this._height;
        var bitmap = new Bitmap(width, height);
        this._windowBackSprite.bitmap = bitmap;
        this._windowBackSprite.setFrame(0, 0, width, height);
        this._windowBackSprite.move(0, 0);

        if (width <= 0 || height <= 0) {
            return;
        }

        var context = bitmap._context;
        var inset = 3;
        var innerWidth = width - inset * 2;
        var innerHeight = height - inset * 2;
        var gradient = context.createLinearGradient(0, inset, 0, height - inset);
        gradient.addColorStop(0, 'rgba(83, 57, 106, 0.94)');
        gradient.addColorStop(1, 'rgba(35, 21, 55, 0.90)');

        context.save();
        drawRoundedPath(context, inset, inset, innerWidth, innerHeight, 13);
        context.fillStyle = gradient;
        context.fill();
        context.lineWidth = 2;
        context.strokeStyle = 'rgba(218, 187, 239, 0.96)';
        context.stroke();

        drawRoundedPath(context, inset + 3, inset + 3, innerWidth - 6, innerHeight - 6, 10);
        context.lineWidth = 1;
        context.strokeStyle = 'rgba(255, 255, 255, 0.16)';
        context.stroke();
        context.restore();
        bitmap._setDirty();
    };

    Window_ATTNamePlate.prototype._refreshFrame = function() {
        var bitmap = new Bitmap(this._width, this._height);
        this._windowFrameSprite.bitmap = bitmap;
        this._windowFrameSprite.setFrame(0, 0, this._width, this._height);
    };

    Window_ATTNamePlate.prototype.refresh = function() {
        var name = $gameMessage.attNamePlate();
        this.resetFontSettings();

        var availableWidth = Math.max(96, Graphics.boxWidth - 24);
        var upperWidth = Math.min(maxWidth, availableWidth);
        var lowerWidth = Math.min(minWidth, upperWidth);
        var measuredWidth = Math.ceil(this.textWidth(name)) +
            this.standardPadding() * 2 + 48;
        var width = clamp(measuredWidth, lowerWidth, upperWidth);

        if (this.width !== width || this.height !== plateHeight) {
            this.move(this.x, this.y, width, plateHeight);
            this.createContents();
        }

        this.contents.clear();
        this.resetFontSettings();
        this.contents.textColor = '#fff7ff';
        this.contents.outlineColor = 'rgba(42, 24, 58, 0.96)';
        this.contents.outlineWidth = 4;
        this.drawText(name, 0, 0, this.contentsWidth(), 'center');
    };

    Window_ATTNamePlate.prototype.updatePlacement = function() {
        var messageWindow = this._messageWindow;
        var maximumX = Math.max(0, Graphics.boxWidth - this.width);
        var maximumY = Math.max(0, Graphics.boxHeight - this.height);
        var x = clamp(messageWindow.x + offsetX, 0, maximumX);
        var y = messageWindow.y - this.height - gap;

        if (y < 0) {
            y = messageWindow.y + messageWindow.height + gap;
        }

        this.x = x;
        this.y = clamp(y, 0, maximumY);
    };

    Window_ATTNamePlate.prototype.openForMessage = function() {
        if ($gameMessage.attNamePlate()) {
            this.refresh();
            this.updatePlacement();
            this.show();
            this.open();
        } else {
            this.close();
        }
    };

    Window_ATTNamePlate.prototype.update = function() {
        Window_Base.prototype.update.call(this);
        if (this.isOpen() || this.isOpening()) {
            this.updatePlacement();
        }
    };

    var _Window_Message_createSubWindows = Window_Message.prototype.createSubWindows;
    Window_Message.prototype.createSubWindows = function() {
        _Window_Message_createSubWindows.call(this);
        this._attNamePlateWindow = new Window_ATTNamePlate(this);
    };

    var _Window_Message_subWindows = Window_Message.prototype.subWindows;
    Window_Message.prototype.subWindows = function() {
        var windows = _Window_Message_subWindows.call(this);
        windows.push(this._attNamePlateWindow);
        return windows;
    };

    var _Window_Message_startMessage = Window_Message.prototype.startMessage;
    Window_Message.prototype.startMessage = function() {
        _Window_Message_startMessage.call(this);
        this._attNamePlateWindow.openForMessage();
    };

    var _Window_Message_updatePlacement = Window_Message.prototype.updatePlacement;
    Window_Message.prototype.updatePlacement = function() {
        _Window_Message_updatePlacement.call(this);
        if (this._attNamePlateWindow) {
            this._attNamePlateWindow.updatePlacement();
        }
    };

    var _Window_Message_terminateMessage = Window_Message.prototype.terminateMessage;
    Window_Message.prototype.terminateMessage = function() {
        this._attNamePlateWindow.close();
        _Window_Message_terminateMessage.call(this);
    };

})();
