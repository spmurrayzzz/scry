window.execDeep = function() {
let jsa = 7;
try {
let first = function(num) {const el = document.createElement('div');el.innerHTML = `<br><div></br><br></div`;return num + el.innerHTML.length;};
let multA = function(num) {return num * 7;};
let second = function(num) {const el = document.createElement('div');el.innerHTML = `<li><div></li><li></div`;return num + el.innerHTML.length;};
let third = function(num) {const el = document.createElement('div');el.innerHTML = `<br><div></br><br></div`;return num + el.innerHTML.length;};
let multB = function(num) {return num * 7;};
let fourth = function(num) {const el = document.createElement('div');el.innerHTML = `<li><div></li><li></div`;return num + el.innerHTML.length;};
let multC = function(num) {return num * 7;};
let fifth = function(num) {const el = document.createElement('div');el.innerHTML = `<br><div></br><br></div`;return num + el.innerHTML.length;};
jsa = third(jsa);jsa = second(jsa);jsa = first(jsa);jsa = multB(jsa);jsa = multC(jsa);jsa = fourth(jsa);jsa = multA(jsa);jsa = fifth(jsa);
} catch (e) { jsa = -1; }
DDG.deep.initialize('/d.js?o=json&jsa=' + jsa, false);
return {isJsaChallenge: true};
};
